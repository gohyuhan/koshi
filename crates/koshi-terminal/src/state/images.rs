//! Image placements that belong to one terminal screen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroU64;
use std::ops::Deref;
use std::sync::Arc;

use koshi_core::geometry::{ImageCellGeometry, Point, Size};
use koshi_sixel::{IndexedImage, SixelPalette};
use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as DeserializerTrait, Serialize};

pub use koshi_image::ImagePlacementError;

use crate::graphics::{
    DecodedAnimation, GraphicsProtocol, ImageAction, ImageDimension, ImageRecord,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};
use crate::grid::state::{Grid, ImagePlaceholder};

mod coverage;
use crate::state::{Screen, TerminalState};
pub(in crate::state) use coverage::discard_native_fragment_references;
pub(super) use coverage::NativeImageSource;

mod kitty;
mod raster;
mod scroll;

#[cfg(test)]
mod tests;

/// The terminal-local identity assigned to a placement without a replacement
/// identity from the protocol.
pub type ImagePlacementId = u64;

/// The maximum number of image placements retained by one terminal state.
pub(crate) const MAX_IMAGE_PLACEMENT_COUNT: usize = 4_096;

/// The maximum RGBA storage retained by one terminal state.
pub(crate) const MAX_IMAGE_STORAGE_BYTE_COUNT: usize = crate::graphics::MAX_IMAGE_BYTE_COUNT;

/// A checked identity for one canonical image source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub(super) struct ImageContentId(NonZeroU64);

impl ImageContentId {
    fn from_raw_image_content_id(raw_image_content_id: u64) -> Option<Self> {
        Some(Self(NonZeroU64::new(raw_image_content_id)?))
    }

    fn get_raw_image_content_id(self) -> u64 {
        self.0.get()
    }
}

/// Decoded pixels and playback state shared by every placement of one image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImageContent {
    image_content_id: ImageContentId,
    decoded_image: Arc<crate::graphics::DecodedImage>,
    animation: Option<Arc<DecodedAnimation>>,
    animation_frame_index: u32,
    animation_loop_count: u32,
    animation_elapsed_nanos: u64,
    is_animation_running: bool,
    is_animation_loading: bool,
    sixel: Option<SixelImageSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SixelImageSource {
    indexed_image: IndexedImage,
    sixel_palette: SixelPalette,
    is_shared_palette: bool,
}

impl SixelImageSource {
    pub(crate) fn from_indexed_image(
        indexed_image: IndexedImage,
        sixel_palette: SixelPalette,
        is_shared_palette: bool,
    ) -> Self {
        Self {
            indexed_image,
            sixel_palette,
            is_shared_palette,
        }
    }

    pub(crate) fn resolve_sixel_image(
        &self,
        shared_sixel_palette: &SixelPalette,
    ) -> Result<Arc<crate::graphics::DecodedImage>, crate::graphics::GraphicsError> {
        let sixel_palette = if self.is_shared_palette {
            shared_sixel_palette
        } else {
            &self.sixel_palette
        };
        Ok(Arc::new(self.indexed_image.resolve_indexed_image(
            sixel_palette,
            sixel_palette.get_register_color(0),
        )?))
    }

    fn with_shared_sixel_palette(&self, sixel_palette: &SixelPalette) -> Self {
        Self {
            indexed_image: self.indexed_image.clone(),
            sixel_palette: sixel_palette.clone(),
            is_shared_palette: self.is_shared_palette,
        }
    }

    fn get_sixel_storage_byte_count(&self) -> Result<usize, String> {
        let pixel_count = usize::try_from(self.indexed_image.get_width_pixels())
            .ok()
            .and_then(|pixel_width| {
                usize::try_from(self.indexed_image.get_height_pixels())
                    .ok()
                    .and_then(|pixel_height| pixel_width.checked_mul(pixel_height))
            })
            .ok_or_else(|| "Sixel source dimensions overflow storage accounting".to_owned())?;
        pixel_count
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|byte_count| byte_count.checked_add(256 * 3))
            .ok_or_else(|| "Sixel source storage count overflows".to_owned())
    }
}

fn compute_animation_storage_byte_count(animation: &DecodedAnimation) -> Result<usize, String> {
    let mut decoded_image_pointers = HashSet::new();
    animation
        .list_frames()
        .iter()
        .try_fold(0usize, |used_byte_count, animation_frame| {
            if !decoded_image_pointers
                .insert(animation_frame.get_decoded_image() as *const crate::graphics::DecodedImage)
            {
                return Ok(used_byte_count);
            }
            used_byte_count
                .checked_add(animation_frame.get_decoded_image().rgba_bytes.len())
                .ok_or_else(|| "animation storage count overflows".to_owned())
        })
}

fn add_image_storage_byte_count(
    storage_byte_count: &mut usize,
    decoded_image_pointers: &mut HashSet<*const crate::graphics::DecodedImage>,
    decoded_image: &crate::graphics::DecodedImage,
) {
    if decoded_image_pointers.insert(decoded_image as *const crate::graphics::DecodedImage) {
        *storage_byte_count = storage_byte_count.saturating_add(decoded_image.rgba_bytes.len());
    }
}

/// A retained Kitty upload and its canonical image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KittyImage {
    image_record: Arc<ImageRecord>,
    image_content: Arc<ImageContent>,
    is_virtual_placement: bool,
    virtual_screen: Option<Screen>,
}

impl serde::Serialize for KittyImage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.image_record.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for KittyImage {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let image_record = Arc::<ImageRecord>::deserialize(deserializer)?;
        let image_content_id =
            ImageContentId::from_raw_image_content_id(1).expect("one is a valid image content id");
        let image_content = Arc::new(ImageContent {
            image_content_id,
            decoded_image: Arc::clone(&image_record.image),
            animation: image_record.animation.clone(),
            animation_frame_index: 0,
            animation_loop_count: 0,
            animation_elapsed_nanos: 0,
            is_animation_running: image_record.animation.is_some(),
            is_animation_loading: false,
            sixel: None,
        });
        Ok(Self {
            image_record,
            image_content,
            is_virtual_placement: false,
            virtual_screen: None,
        })
    }
}

pub(super) fn legacy_image_content_for_record(
    image_record: &Arc<ImageRecord>,
) -> Arc<ImageContent> {
    Arc::new(ImageContent {
        image_content_id: ImageContentId::from_raw_image_content_id(1)
            .expect("one is a valid image content id"),
        decoded_image: Arc::clone(&image_record.image),
        animation: image_record.animation.clone(),
        animation_frame_index: 0,
        animation_loop_count: 0,
        animation_elapsed_nanos: 0,
        is_animation_running: image_record.animation.is_some(),
        is_animation_loading: false,
        sixel: None,
    })
}

impl Deref for KittyImage {
    type Target = ImageRecord;

    fn deref(&self) -> &Self::Target {
        self.image_record.as_ref()
    }
}

/// The geometry and pixel transform needed to rebuild one prepared raster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RasterPlan {
    /// The complete cell rectangle and the currently clipped source offset.
    geometry: ImageCellGeometry,
    /// The source rectangle in canonical image pixels.
    source_rect: (u32, u32, u32, u32),
    /// The fitted source size in output pixels.
    target_size: (u32, u32),
    /// The complete output canvas size in pixels.
    canvas_size: (u32, u32),
    /// The target's top-left pixel offset in the complete canvas.
    pixel_offset: (u32, u32),
}

impl RasterPlan {
    fn is_same_raster(&self, other: &Self) -> bool {
        self.source_rect == other.source_rect
            && self.target_size == other.target_size
            && self.canvas_size == other.canvas_size
            && self.pixel_offset == other.pixel_offset
    }
}

#[derive(Debug, Clone)]
struct ImageStateSnapshot {
    native_images: Vec<NativeImageSource>,
    native_fragment_count_by_image_source_id: HashMap<u64, usize>,
    primary_image_placements: Vec<ImagePlacement>,
    primary_image_history: Vec<PrimaryHistoryImagePlacement>,
    alternate_image_placements: Vec<ImagePlacement>,
    kitty_images: Vec<KittyImage>,
    next_image_placement_id: ImagePlacementId,
    next_image_content_id: ImageContentId,
}

enum ImageCursorMovement {
    RegularCursorMove {
        column_count: u16,
        row_count: u16,
    },
    SixelCursorMove {
        cursor_row: u16,
        cursor_column: u16,
        column_count: u16,
        should_move_cursor_right: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImageRecord {
    protocol: GraphicsProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decoded_image: Option<Arc<crate::graphics::DecodedImage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    animation: Option<Arc<DecodedAnimation>>,
    action: ImageAction,
    display: crate::graphics::ImageDisplay,
    anchor: (u16, u16),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImagePlacement {
    image_placement_id: ImagePlacementId,
    image_record: SerializedImageRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_content_id: Option<ImageContentId>,
    anchor: (u64, u16),
    column_count: u16,
    row_count: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<RasterPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    geometry: Option<ImageCellGeometry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raster: Option<Arc<crate::graphics::DecodedImage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedKittyImage {
    #[serde(flatten)]
    image_record: SerializedImageRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_content_id: Option<ImageContentId>,
    #[serde(default, skip_serializing_if = "is_false_boolean_flag")]
    is_virtual_placement: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    virtual_screen: Option<Screen>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImageContent {
    image_content_id: ImageContentId,
    decoded_image: Arc<crate::graphics::DecodedImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    animation: Option<Arc<DecodedAnimation>>,
    #[serde(default, skip_serializing_if = "is_zero_numeric_field")]
    animation_frame_index: u32,
    #[serde(default, skip_serializing_if = "is_zero_numeric_field")]
    animation_loop_count: u32,
    #[serde(default, skip_serializing_if = "is_zero_numeric_field")]
    animation_elapsed_nanos: u64,
    #[serde(default, skip_serializing_if = "is_false_boolean_flag")]
    is_animation_running: bool,
    #[serde(default, skip_serializing_if = "is_false_boolean_flag")]
    is_animation_loading: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sixel: Option<SixelImageSource>,
}

fn is_zero_numeric_field(numeric_field: &(impl PartialEq + Default)) -> bool {
    numeric_field == &Default::default()
}

fn is_false_boolean_flag(boolean_flag: &bool) -> bool {
    !*boolean_flag
}

pub(super) struct RestoredImageState {
    pub(super) primary_image_placements: Vec<ImagePlacement>,
    pub(super) primary_image_history: Vec<PrimaryHistoryImagePlacement>,
    pub(super) alternate_image_placements: Vec<ImagePlacement>,
    pub(super) kitty_images: Vec<KittyImage>,
    pub(super) next_image_content_id: ImageContentId,
}

type RasterCacheKey = (
    ImageContentId,
    (u32, u32, u32, u32),
    (u32, u32),
    (u32, u32),
    (u32, u32),
);

/// The aggregate byte budget charged while one terminal state is decoded.
pub(super) struct ImageStateBudget {
    used_byte_count: usize,
    byte_limit: usize,
}

fn charge_animation_budget(
    budget: &mut ImageStateBudget,
    animation: Option<&Arc<DecodedAnimation>>,
) -> Result<(), String> {
    if let Some(animation) = animation {
        budget.charge_byte_count(compute_animation_storage_byte_count(animation)?)?;
    }
    Ok(())
}

impl ImageStateBudget {
    pub(super) fn new() -> Self {
        Self::with_byte_limit(MAX_IMAGE_STORAGE_BYTE_COUNT)
    }

    pub(super) fn with_byte_limit(byte_limit: usize) -> Self {
        Self {
            used_byte_count: 0,
            byte_limit,
        }
    }

    fn get_remaining_byte_count(&self) -> usize {
        self.byte_limit.saturating_sub(self.used_byte_count)
    }

    fn charge_byte_count(&mut self, requested_byte_count: usize) -> Result<(), String> {
        let total_byte_count = self
            .used_byte_count
            .checked_add(requested_byte_count)
            .ok_or_else(|| {
                ImagePlacementError::StorageLimit {
                    used_byte_count: self.used_byte_count,
                    requested_byte_count,
                    byte_limit: self.byte_limit,
                }
                .to_string()
            })?;
        if total_byte_count > self.byte_limit {
            return Err(ImagePlacementError::StorageLimit {
                used_byte_count: self.used_byte_count,
                requested_byte_count,
                byte_limit: self.byte_limit,
            }
            .to_string());
        }
        self.used_byte_count = total_byte_count;
        Ok(())
    }

    fn release_byte_count(&mut self, released_byte_count: usize) {
        self.used_byte_count = self.used_byte_count.saturating_sub(released_byte_count);
    }
}

fn alias_animation_image(
    decoded_image: &mut Option<Arc<crate::graphics::DecodedImage>>,
    animation: Option<&Arc<DecodedAnimation>>,
    animation_frame_index: u32,
    budget: &mut ImageStateBudget,
) -> Result<(), String> {
    let (Some(current_image), Some(animation)) = (decoded_image.as_ref(), animation) else {
        return Ok(());
    };
    let animation_frame_index_usize = usize::try_from(animation_frame_index).map_err(|_| {
        ImagePlacementError::AnimationFrameNotFound {
            frame_index: animation_frame_index,
        }
        .to_string()
    })?;
    let animation_frame_image = animation
        .list_frames()
        .get(animation_frame_index_usize)
        .ok_or_else(|| {
            ImagePlacementError::AnimationFrameNotFound {
                frame_index: animation_frame_index,
            }
            .to_string()
        })?
        .clone_decoded_image();
    if current_image.as_ref() != animation_frame_image.as_ref() {
        return Err("image content pixels do not match its current animation frame".to_owned());
    }
    budget.release_byte_count(current_image.rgba_bytes.capacity());
    *decoded_image = Some(animation_frame_image);
    Ok(())
}

struct BudgetedImageBytesSeed {
    byte_limit: usize,
}

impl<'de> DeserializeSeed<'de> for BudgetedImageBytesSeed {
    type Value = Vec<u8>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_seq(BudgetedImageBytesVisitor {
            byte_limit: self.byte_limit,
        })
    }
}

struct BudgetedImageBytesVisitor {
    byte_limit: usize,
}

impl<'de> Visitor<'de> for BudgetedImageBytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image byte sequence")
    }

    fn visit_seq<SequenceAccess>(
        self,
        mut sequence: SequenceAccess,
    ) -> Result<Self::Value, SequenceAccess::Error>
    where
        SequenceAccess: SeqAccess<'de>,
    {
        let mut image_bytes = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(self.byte_limit),
        );
        while let Some(image_byte) = sequence.next_element::<u8>()? {
            if image_bytes.len() == self.byte_limit {
                return Err(de::Error::custom(format!(
                    "image state RGBA data exceeds the remaining storage budget of {} bytes",
                    self.byte_limit
                )));
            }
            image_bytes.push(image_byte);
        }
        Ok(image_bytes)
    }

    fn visit_bytes<Error>(self, image_bytes: &[u8]) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        if image_bytes.len() > self.byte_limit {
            return Err(Error::custom(format!(
                "image state RGBA data exceeds the remaining storage budget of {} bytes",
                self.byte_limit
            )));
        }
        Ok(image_bytes.to_vec())
    }

    fn visit_byte_buf<Error>(self, image_bytes: Vec<u8>) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        if image_bytes.len() > self.byte_limit {
            return Err(Error::custom(format!(
                "image state RGBA data exceeds the remaining storage budget of {} bytes",
                self.byte_limit
            )));
        }
        Ok(image_bytes)
    }
}

pub(super) struct BudgetedDecodedImageSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for BudgetedDecodedImageSeed<'_> {
    type Value = Arc<crate::graphics::DecodedImage>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(BudgetedDecodedImageVisitor {
            budget: self.budget,
        })
    }
}

struct BudgetedDecodedImageVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for BudgetedDecodedImageVisitor<'_> {
    type Value = Arc<crate::graphics::DecodedImage>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded decoded RGBA image")
    }

    fn visit_map<MapAccess>(
        self,
        mut map_access: MapAccess,
    ) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut pixel_width = None;
        let mut pixel_height = None;
        let mut rgba_bytes = None;
        while let Some(field_name) = map_access.next_key::<String>()? {
            match field_name.as_str() {
                "pixel_width" => {
                    if pixel_width.is_some() {
                        return Err(de::Error::duplicate_field("pixel_width"));
                    }
                    pixel_width = Some(map_access.next_value::<u32>()?);
                }
                "pixel_height" => {
                    if pixel_height.is_some() {
                        return Err(de::Error::duplicate_field("pixel_height"));
                    }
                    pixel_height = Some(map_access.next_value::<u32>()?);
                }
                "rgba_bytes" => {
                    if rgba_bytes.is_some() {
                        return Err(de::Error::duplicate_field("rgba_bytes"));
                    }
                    let decoded_rgba_bytes =
                        map_access.next_value_seed(BudgetedImageBytesSeed {
                            byte_limit: self.budget.get_remaining_byte_count(),
                        })?;
                    self.budget
                        .charge_byte_count(decoded_rgba_bytes.capacity())
                        .map_err(de::Error::custom)?;
                    rgba_bytes = Some(decoded_rgba_bytes);
                }
                _ => {
                    let _: de::IgnoredAny = map_access.next_value()?;
                }
            }
        }
        let pixel_width = pixel_width.ok_or_else(|| de::Error::missing_field("pixel_width"))?;
        let pixel_height = pixel_height.ok_or_else(|| de::Error::missing_field("pixel_height"))?;
        let rgba_bytes = rgba_bytes.ok_or_else(|| de::Error::missing_field("rgba_bytes"))?;
        let decoded_image = Arc::new(crate::graphics::DecodedImage {
            pixel_width,
            pixel_height,
            rgba_bytes,
        });
        validate_image_pixels(&decoded_image).map_err(de::Error::custom)?;
        Ok(decoded_image)
    }
}

struct OptionalImageSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for OptionalImageSeed<'_> {
    type Value = Option<Arc<crate::graphics::DecodedImage>>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_option(OptionalImageVisitor {
            budget: self.budget,
        })
    }
}

struct OptionalImageVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for OptionalImageVisitor<'_> {
    type Value = Option<Arc<crate::graphics::DecodedImage>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an optional decoded RGBA image")
    }

    fn visit_none<Error>(self) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<Error>(self) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        Ok(None)
    }

    fn visit_some<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        BudgetedDecodedImageSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
        .map(Some)
    }
}

pub(super) struct SerializedImagePlacementsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImagePlacementsSeed<'_> {
    type Value = Vec<SerializedImagePlacement>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_seq(SerializedImagePlacementsVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedImagePlacementsVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedImagePlacementsVisitor<'_> {
    type Value = Vec<SerializedImagePlacement>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image placement sequence")
    }

    fn visit_seq<SequenceAccess>(
        self,
        mut sequence: SequenceAccess,
    ) -> Result<Self::Value, SequenceAccess::Error>
    where
        SequenceAccess: SeqAccess<'de>,
    {
        let mut serialized_placements = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENT_COUNT),
        );
        while serialized_placements.len() < MAX_IMAGE_PLACEMENT_COUNT {
            let Some(serialized_placement) =
                sequence.next_element_seed(SerializedImagePlacementSeed {
                    budget: self.budget,
                })?
            else {
                return Ok(serialized_placements);
            };
            serialized_placements.push(serialized_placement);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            }));
        }
        Ok(serialized_placements)
    }
}

pub(super) struct SerializedKittyImagesSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedKittyImagesSeed<'_> {
    type Value = Vec<SerializedKittyImage>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_seq(SerializedKittyImagesVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedKittyImagesVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedKittyImagesVisitor<'_> {
    type Value = Vec<SerializedKittyImage>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded Kitty image sequence")
    }

    fn visit_seq<SequenceAccess>(
        self,
        mut sequence: SequenceAccess,
    ) -> Result<Self::Value, SequenceAccess::Error>
    where
        SequenceAccess: SeqAccess<'de>,
    {
        let mut serialized_kitty_images = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENT_COUNT),
        );
        while serialized_kitty_images.len() < MAX_IMAGE_PLACEMENT_COUNT {
            let Some(serialized_kitty_image) =
                sequence.next_element_seed(SerializedKittyImageSeed {
                    budget: self.budget,
                })?
            else {
                return Ok(serialized_kitty_images);
            };
            serialized_kitty_images.push(serialized_kitty_image);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            }));
        }
        Ok(serialized_kitty_images)
    }
}

pub(super) struct SerializedImageContentsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImageContentsSeed<'_> {
    type Value = Vec<SerializedImageContent>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_seq(SerializedImageContentsVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedImageContentsVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedImageContentsVisitor<'_> {
    type Value = Vec<SerializedImageContent>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image content sequence")
    }

    fn visit_seq<SequenceAccess>(
        self,
        mut sequence: SequenceAccess,
    ) -> Result<Self::Value, SequenceAccess::Error>
    where
        SequenceAccess: SeqAccess<'de>,
    {
        let mut serialized_contents = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENT_COUNT),
        );
        while serialized_contents.len() < MAX_IMAGE_PLACEMENT_COUNT {
            let Some(serialized_content) =
                sequence.next_element_seed(SerializedImageContentSeed {
                    budget: self.budget,
                })?
            else {
                return Ok(serialized_contents);
            };
            serialized_contents.push(serialized_content);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            }));
        }
        Ok(serialized_contents)
    }
}

struct SerializedImageRecordSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImageRecordSeed<'_> {
    type Value = SerializedImageRecord;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(SerializedImageRecordVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedImageRecordVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedImageRecordVisitor<'_> {
    type Value = SerializedImageRecord;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an image record")
    }

    fn visit_map<MapAccess>(self, mut map: MapAccess) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut graphics_protocol = None;
        let mut decoded_image = None;
        let mut decoded_animation = None;
        let mut image_action = None;
        let mut image_display = None;
        let mut image_anchor = None;
        while let Some(field_name) = map.next_key::<String>()? {
            match field_name.as_str() {
                "protocol" => {
                    if graphics_protocol.is_some() {
                        return Err(de::Error::duplicate_field("protocol"));
                    }
                    graphics_protocol = Some(map.next_value::<GraphicsProtocol>()?);
                }
                "decoded_image" => {
                    if decoded_image.is_some() {
                        return Err(de::Error::duplicate_field("decoded_image"));
                    }
                    decoded_image = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if decoded_animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    decoded_animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "action" => {
                    if image_action.is_some() {
                        return Err(de::Error::duplicate_field("action"));
                    }
                    image_action = Some(map.next_value::<ImageAction>()?);
                }
                "display" => {
                    if image_display.is_some() {
                        return Err(de::Error::duplicate_field("display"));
                    }
                    image_display = Some(map.next_value::<crate::graphics::ImageDisplay>()?);
                }
                "anchor" => {
                    if image_anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    image_anchor = Some(map.next_value::<(u16, u16)>()?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let mut decoded_image = decoded_image.unwrap_or_default();
        let decoded_animation = decoded_animation.unwrap_or_default();
        alias_animation_image(
            &mut decoded_image,
            decoded_animation.as_ref(),
            0,
            self.budget,
        )
        .map_err(de::Error::custom)?;
        charge_animation_budget(self.budget, decoded_animation.as_ref())
            .map_err(de::Error::custom)?;
        Ok(SerializedImageRecord {
            protocol: graphics_protocol.ok_or_else(|| de::Error::missing_field("protocol"))?,
            decoded_image,
            animation: decoded_animation,
            action: image_action.ok_or_else(|| de::Error::missing_field("action"))?,
            display: image_display.ok_or_else(|| de::Error::missing_field("display"))?,
            anchor: image_anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
        })
    }
}

struct SerializedImagePlacementSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImagePlacementSeed<'_> {
    type Value = SerializedImagePlacement;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(SerializedImagePlacementVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedImagePlacementVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedImagePlacementVisitor<'_> {
    type Value = SerializedImagePlacement;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an image placement")
    }

    fn visit_map<MapAccess>(self, mut map: MapAccess) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut image_placement_id = None;
        let mut serialized_image_record = None;
        let mut image_content_id = None;
        let mut image_anchor = None;
        let mut column_count = None;
        let mut row_count = None;
        let mut raster_plan = None;
        let mut image_geometry = None;
        let mut raster_image = None;
        while let Some(field_name) = map.next_key::<String>()? {
            match field_name.as_str() {
                "image_placement_id" => {
                    if image_placement_id.is_some() {
                        return Err(de::Error::duplicate_field("image_placement_id"));
                    }
                    image_placement_id = Some(map.next_value::<ImagePlacementId>()?);
                }
                "image_record" => {
                    if serialized_image_record.is_some() {
                        return Err(de::Error::duplicate_field("image_record"));
                    }
                    serialized_image_record =
                        Some(map.next_value_seed(SerializedImageRecordSeed {
                            budget: self.budget,
                        })?);
                }
                "image_content_id" => {
                    if image_content_id.is_some() {
                        return Err(de::Error::duplicate_field("image_content_id"));
                    }
                    image_content_id = Some(map.next_value::<Option<ImageContentId>>()?);
                }
                "anchor" => {
                    if image_anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    image_anchor = Some(map.next_value::<(u64, u16)>()?);
                }
                "column_count" => {
                    if column_count.is_some() {
                        return Err(de::Error::duplicate_field("column_count"));
                    }
                    column_count = Some(map.next_value::<u16>()?);
                }
                "row_count" => {
                    if row_count.is_some() {
                        return Err(de::Error::duplicate_field("row_count"));
                    }
                    row_count = Some(map.next_value::<u16>()?);
                }
                "plan" => {
                    if raster_plan.is_some() {
                        return Err(de::Error::duplicate_field("plan"));
                    }
                    raster_plan = Some(map.next_value::<Option<RasterPlan>>()?);
                }
                "geometry" => {
                    if image_geometry.is_some() {
                        return Err(de::Error::duplicate_field("geometry"));
                    }
                    image_geometry = Some(map.next_value::<Option<ImageCellGeometry>>()?);
                }
                "raster" => {
                    if raster_image.is_some() {
                        return Err(de::Error::duplicate_field("raster"));
                    }
                    raster_image = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(SerializedImagePlacement {
            image_placement_id: image_placement_id
                .ok_or_else(|| de::Error::missing_field("image_placement_id"))?,
            image_record: serialized_image_record
                .ok_or_else(|| de::Error::missing_field("image_record"))?,
            image_content_id: image_content_id.unwrap_or_default(),
            anchor: image_anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
            column_count: column_count.ok_or_else(|| de::Error::missing_field("column_count"))?,
            row_count: row_count.ok_or_else(|| de::Error::missing_field("row_count"))?,
            plan: raster_plan.unwrap_or_default(),
            geometry: image_geometry.unwrap_or_default(),
            raster: raster_image.unwrap_or_default(),
        })
    }
}

struct SerializedKittyImageSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedKittyImageSeed<'_> {
    type Value = SerializedKittyImage;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(SerializedKittyImageVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedKittyImageVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedKittyImageVisitor<'_> {
    type Value = SerializedKittyImage;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a retained Kitty image")
    }

    fn visit_map<MapAccess>(self, mut map: MapAccess) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut graphics_protocol = None;
        let mut decoded_image = None;
        let mut decoded_animation = None;
        let mut image_action = None;
        let mut image_display = None;
        let mut image_anchor = None;
        let mut image_content_id = None;
        let mut is_virtual_placement = None;
        let mut virtual_screen = None;
        while let Some(field_name) = map.next_key::<String>()? {
            match field_name.as_str() {
                "protocol" => {
                    if graphics_protocol.is_some() {
                        return Err(de::Error::duplicate_field("protocol"));
                    }
                    graphics_protocol = Some(map.next_value::<GraphicsProtocol>()?);
                }
                "decoded_image" => {
                    if decoded_image.is_some() {
                        return Err(de::Error::duplicate_field("decoded_image"));
                    }
                    decoded_image = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if decoded_animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    decoded_animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "action" => {
                    if image_action.is_some() {
                        return Err(de::Error::duplicate_field("action"));
                    }
                    image_action = Some(map.next_value::<ImageAction>()?);
                }
                "display" => {
                    if image_display.is_some() {
                        return Err(de::Error::duplicate_field("display"));
                    }
                    image_display = Some(map.next_value::<crate::graphics::ImageDisplay>()?);
                }
                "anchor" => {
                    if image_anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    image_anchor = Some(map.next_value::<(u16, u16)>()?);
                }
                "image_content_id" => {
                    if image_content_id.is_some() {
                        return Err(de::Error::duplicate_field("image_content_id"));
                    }
                    image_content_id = Some(map.next_value::<Option<ImageContentId>>()?);
                }
                "is_virtual_placement" => {
                    if is_virtual_placement.is_some() {
                        return Err(de::Error::duplicate_field("is_virtual_placement"));
                    }
                    is_virtual_placement = Some(map.next_value::<bool>()?);
                }
                "virtual_screen" => {
                    if virtual_screen.is_some() {
                        return Err(de::Error::duplicate_field("virtual_screen"));
                    }
                    virtual_screen = Some(map.next_value::<Option<Screen>>()?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let mut decoded_image = decoded_image.unwrap_or_default();
        let decoded_animation = decoded_animation.unwrap_or_default();
        alias_animation_image(
            &mut decoded_image,
            decoded_animation.as_ref(),
            0,
            self.budget,
        )
        .map_err(de::Error::custom)?;
        charge_animation_budget(self.budget, decoded_animation.as_ref())
            .map_err(de::Error::custom)?;
        Ok(SerializedKittyImage {
            image_record: SerializedImageRecord {
                protocol: graphics_protocol.ok_or_else(|| de::Error::missing_field("protocol"))?,
                decoded_image,
                animation: decoded_animation,
                action: image_action.ok_or_else(|| de::Error::missing_field("action"))?,
                display: image_display.ok_or_else(|| de::Error::missing_field("display"))?,
                anchor: image_anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
            },
            image_content_id: image_content_id.unwrap_or_default(),
            is_virtual_placement: is_virtual_placement.unwrap_or(false),
            virtual_screen: virtual_screen.unwrap_or_default(),
        })
    }
}

struct SerializedImageContentSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImageContentSeed<'_> {
    type Value = SerializedImageContent;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(SerializedImageContentVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedImageContentVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedImageContentVisitor<'_> {
    type Value = SerializedImageContent;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an image content entry")
    }

    fn visit_map<MapAccess>(self, mut map: MapAccess) -> Result<Self::Value, MapAccess::Error>
    where
        MapAccess: serde::de::MapAccess<'de>,
    {
        let mut image_content_id = None;
        let mut decoded_image = None;
        let mut decoded_animation = None;
        let mut animation_frame_index = None;
        let mut animation_loop_count = None;
        let mut animation_elapsed_nanos = None;
        let mut is_animation_running = None;
        let mut is_animation_loading = None;
        let mut sixel = None;
        while let Some(field_name) = map.next_key::<String>()? {
            match field_name.as_str() {
                "image_content_id" => {
                    if image_content_id.is_some() {
                        return Err(de::Error::duplicate_field("image_content_id"));
                    }
                    image_content_id = Some(map.next_value::<ImageContentId>()?);
                }
                "decoded_image" => {
                    if decoded_image.is_some() {
                        return Err(de::Error::duplicate_field("decoded_image"));
                    }
                    decoded_image = Some(map.next_value_seed(BudgetedDecodedImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if decoded_animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    decoded_animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "animation_frame_index" => {
                    if animation_frame_index.is_some() {
                        return Err(de::Error::duplicate_field("animation_frame_index"));
                    }
                    animation_frame_index = Some(map.next_value::<u32>()?);
                }
                "animation_loop_count" => {
                    if animation_loop_count.is_some() {
                        return Err(de::Error::duplicate_field("animation_loop_count"));
                    }
                    animation_loop_count = Some(map.next_value::<u32>()?);
                }
                "animation_elapsed_nanos" => {
                    if animation_elapsed_nanos.is_some() {
                        return Err(de::Error::duplicate_field("animation_elapsed_nanos"));
                    }
                    animation_elapsed_nanos = Some(map.next_value::<u64>()?);
                }
                "is_animation_running" => {
                    if is_animation_running.is_some() {
                        return Err(de::Error::duplicate_field("is_animation_running"));
                    }
                    is_animation_running = Some(map.next_value::<bool>()?);
                }
                "is_animation_loading" => {
                    if is_animation_loading.is_some() {
                        return Err(de::Error::duplicate_field("is_animation_loading"));
                    }
                    is_animation_loading = Some(map.next_value::<bool>()?);
                }
                "sixel" => {
                    if sixel.is_some() {
                        return Err(de::Error::duplicate_field("sixel"));
                    }
                    sixel = Some(map.next_value::<Option<SixelImageSource>>()?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let sixel = sixel.unwrap_or_default();
        let decoded_animation = decoded_animation.unwrap_or_default();
        let animation_frame_index = animation_frame_index.unwrap_or_default();
        let animation_loop_count = animation_loop_count.unwrap_or_default();
        let animation_elapsed_nanos = animation_elapsed_nanos.unwrap_or_default();
        let is_animation_running = is_animation_running.unwrap_or(false);
        let is_animation_loading = is_animation_loading.unwrap_or(false);
        let mut decoded_image =
            Some(decoded_image.ok_or_else(|| de::Error::missing_field("decoded_image"))?);
        validate_animation_content_state(
            decoded_image.as_ref().expect("the image is present"),
            decoded_animation.as_ref(),
            animation_frame_index,
            animation_loop_count,
            animation_elapsed_nanos,
            is_animation_running,
            is_animation_loading,
        )
        .map_err(de::Error::custom)?;
        alias_animation_image(
            &mut decoded_image,
            decoded_animation.as_ref(),
            animation_frame_index,
            self.budget,
        )
        .map_err(de::Error::custom)?;
        charge_animation_budget(self.budget, decoded_animation.as_ref())
            .map_err(de::Error::custom)?;
        if let Some(sixel_source) = &sixel {
            let storage_byte_count = sixel_source
                .get_sixel_storage_byte_count()
                .map_err(de::Error::custom)?;
            self.budget
                .charge_byte_count(storage_byte_count)
                .map_err(de::Error::custom)?;
        }
        Ok(SerializedImageContent {
            image_content_id: image_content_id
                .ok_or_else(|| de::Error::missing_field("image_content_id"))?,
            decoded_image: decoded_image.expect("the image is present"),
            animation: decoded_animation,
            animation_frame_index,
            animation_loop_count,
            animation_elapsed_nanos,
            is_animation_running,
            is_animation_loading,
            sixel,
        })
    }
}

pub(super) struct OptionalContentsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for OptionalContentsSeed<'_> {
    type Value = Option<Vec<SerializedImageContent>>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_option(OptionalContentsVisitor {
            budget: self.budget,
        })
    }
}

struct OptionalContentsVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for OptionalContentsVisitor<'_> {
    type Value = Option<Vec<SerializedImageContent>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an optional image content sequence")
    }

    fn visit_none<Error>(self) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<Error>(self) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        Ok(None)
    }

    fn visit_some<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        SerializedImageContentsSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
        .map(Some)
    }
}

impl SerializedImageRecord {
    fn from_image_record(image_record: &ImageRecord, should_include_image: bool) -> Self {
        SerializedImageRecord {
            protocol: image_record.protocol,
            decoded_image: should_include_image.then(|| Arc::clone(&image_record.image)),
            animation: should_include_image
                .then(|| image_record.animation.clone())
                .flatten(),
            action: image_record.action,
            display: image_record.display.clone(),
            anchor: image_record.anchor,
        }
    }

    fn into_image_record(
        self,
        image_content: Arc<ImageContent>,
    ) -> Result<Arc<ImageRecord>, String> {
        if let Some(decoded_image) = self.decoded_image {
            if decoded_image.as_ref() != image_content.decoded_image.as_ref() {
                return Err("image record pixels do not match its content id".to_owned());
            }
        }
        if let Some(animation) = &self.animation {
            if image_content.animation.as_deref() != Some(animation.as_ref()) {
                return Err("image record animation does not match its content id".to_owned());
            }
        }
        Ok(Arc::new(ImageRecord {
            protocol: self.protocol,
            image: Arc::clone(&image_content.decoded_image),
            animation: image_content.animation.clone(),
            action: self.action,
            display: self.display,
            anchor: self.anchor,
        }))
    }
}

pub(super) fn serialize_image_placement(
    placement: &ImagePlacement,
) -> Result<SerializedImagePlacement, String> {
    if !Arc::ptr_eq(
        &placement.image_record.image,
        &placement.image_content.decoded_image,
    ) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    Ok(SerializedImagePlacement {
        image_placement_id: placement.image_placement_id,
        image_record: SerializedImageRecord::from_image_record(placement.get_image_record(), false),
        image_content_id: Some(placement.image_content.image_content_id),
        anchor: (u64::from(placement.anchor.0), placement.anchor.1),
        column_count: placement.column_count,
        row_count: placement.row_count,
        plan: Some(placement.plan.clone()),
        geometry: None,
        raster: None,
    })
}

pub(super) fn serialize_primary_history_image_placement(
    placement: &PrimaryHistoryImagePlacement,
) -> Result<SerializedImagePlacement, String> {
    if !Arc::ptr_eq(
        &placement.image_record.image,
        &placement.image_content.decoded_image,
    ) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    Ok(SerializedImagePlacement {
        image_placement_id: placement.image_placement_id,
        image_record: SerializedImageRecord::from_image_record(
            placement.image_record.as_ref(),
            false,
        ),
        image_content_id: Some(placement.image_content.image_content_id),
        anchor: placement.anchor,
        column_count: placement.column_count,
        row_count: placement.row_count,
        plan: Some(placement.plan.clone()),
        geometry: None,
        raster: None,
    })
}

pub(super) fn serialize_kitty_image(
    kitty_image: &KittyImage,
) -> Result<SerializedKittyImage, String> {
    if !Arc::ptr_eq(
        &kitty_image.image_record.image,
        &kitty_image.image_content.decoded_image,
    ) {
        return Err("Kitty image record is not aliased to its content".to_owned());
    }
    Ok(SerializedKittyImage {
        image_record: SerializedImageRecord::from_image_record(
            kitty_image.image_record.as_ref(),
            false,
        ),
        image_content_id: Some(kitty_image.image_content.image_content_id),
        is_virtual_placement: kitty_image.is_virtual_placement,
        virtual_screen: kitty_image.virtual_screen,
    })
}

pub(super) fn serialize_image_content_table(
    terminal_state: &TerminalState,
) -> Result<Vec<SerializedImageContent>, String> {
    let mut image_content_by_id = BTreeMap::<ImageContentId, Arc<ImageContent>>::new();
    let mut add_image_content = |image_content: &Arc<ImageContent>| -> Result<(), String> {
        if let Some(existing_image_content) =
            image_content_by_id.get(&image_content.image_content_id)
        {
            if !Arc::ptr_eq(existing_image_content, image_content)
                || !Arc::ptr_eq(
                    &existing_image_content.decoded_image,
                    &image_content.decoded_image,
                )
            {
                return Err("one image content identity has different pixels".to_owned());
            }
        } else {
            if image_content_by_id.len() == MAX_IMAGE_PLACEMENT_COUNT {
                return Err(ImagePlacementError::TooManyPlacements {
                    placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                    placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
                }
                .to_string());
            }
            image_content_by_id.insert(image_content.image_content_id, Arc::clone(image_content));
        }
        Ok(())
    };
    for placement in terminal_state.primary_image_placements.iter().chain(
        terminal_state
            .native_images
            .iter()
            .map(|native_image_source| &native_image_source.placement),
    ) {
        add_image_content(&placement.image_content)?;
    }
    for placement in &terminal_state.primary_image_history {
        add_image_content(&placement.image_content)?;
    }
    for placement in &terminal_state.alternate_image_placements {
        add_image_content(&placement.image_content)?;
    }
    for kitty_image in &terminal_state.kitty_images {
        add_image_content(&kitty_image.image_content)?;
    }
    Ok(image_content_by_id
        .into_iter()
        .map(|(image_content_id, image_content)| SerializedImageContent {
            image_content_id,
            decoded_image: Arc::clone(&image_content.decoded_image),
            animation: image_content.animation.clone(),
            animation_frame_index: image_content.animation_frame_index,
            animation_loop_count: image_content.animation_loop_count,
            animation_elapsed_nanos: image_content.animation_elapsed_nanos,
            is_animation_running: image_content.is_animation_running,
            is_animation_loading: image_content.is_animation_loading,
            sixel: image_content.sixel.clone(),
        })
        .collect())
}

struct RestoreImageBuilder {
    image_content_by_id: BTreeMap<ImageContentId, Arc<ImageContent>>,
    legacy_kitty_content_by_image_id: BTreeMap<u32, Arc<ImageContent>>,
    referenced_image_content_ids: HashSet<ImageContentId>,
    raster_by_key: HashMap<RasterCacheKey, Arc<crate::graphics::DecodedImage>>,
    decoded_image_pointers: HashSet<*const crate::graphics::DecodedImage>,
    storage_byte_count: usize,
    next_image_content_id: ImageContentId,
    is_new_format: bool,
}

impl RestoreImageBuilder {
    fn from_serialized_image_contents(
        serialized_contents: Option<Vec<SerializedImageContent>>,
        next_image_content_id: Option<ImageContentId>,
    ) -> Result<Self, String> {
        let is_new_format = serialized_contents.is_some();
        let mut image_content_by_id = BTreeMap::new();
        let mut storage_byte_count = 0usize;
        let mut decoded_image_pointers = HashSet::new();
        for serialized_content in serialized_contents.unwrap_or_default() {
            if image_content_by_id.contains_key(&serialized_content.image_content_id) {
                return Err("image content identities must be unique".to_owned());
            }
            if image_content_by_id.len() == MAX_IMAGE_PLACEMENT_COUNT {
                return Err(ImagePlacementError::TooManyPlacements {
                    placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                    placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
                }
                .to_string());
            }
            validate_animation_content_state(
                &serialized_content.decoded_image,
                serialized_content.animation.as_ref(),
                serialized_content.animation_frame_index,
                serialized_content.animation_loop_count,
                serialized_content.animation_elapsed_nanos,
                serialized_content.is_animation_running,
                serialized_content.is_animation_loading,
            )?;
            let decoded_image = if let Some(animation) = &serialized_content.animation {
                animation.list_frames()[serialized_content.animation_frame_index as usize]
                    .clone_decoded_image()
            } else {
                serialized_content.decoded_image
            };
            image_content_by_id.insert(
                serialized_content.image_content_id,
                Arc::new(ImageContent {
                    image_content_id: serialized_content.image_content_id,
                    decoded_image,
                    animation: serialized_content.animation,
                    animation_frame_index: serialized_content.animation_frame_index,
                    animation_loop_count: serialized_content.animation_loop_count,
                    animation_elapsed_nanos: serialized_content.animation_elapsed_nanos,
                    is_animation_running: serialized_content.is_animation_running,
                    is_animation_loading: serialized_content.is_animation_loading,
                    sixel: serialized_content.sixel,
                }),
            );
            add_image_storage_byte_count(
                &mut storage_byte_count,
                &mut decoded_image_pointers,
                image_content_by_id[&serialized_content.image_content_id]
                    .decoded_image
                    .as_ref(),
            );
            if let Some(animation) =
                &image_content_by_id[&serialized_content.image_content_id].animation
            {
                for animation_frame in animation.list_frames() {
                    add_image_storage_byte_count(
                        &mut storage_byte_count,
                        &mut decoded_image_pointers,
                        animation_frame.get_decoded_image(),
                    );
                }
            }
            if let Some(sixel_source) =
                &image_content_by_id[&serialized_content.image_content_id].sixel
            {
                storage_byte_count = storage_byte_count
                    .checked_add(sixel_source.get_sixel_storage_byte_count()?)
                    .ok_or_else(|| "image content storage count overflows".to_owned())?;
            }
            if storage_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT {
                return Err("image content storage exceeds the image limit".to_owned());
            }
        }
        let next_image_content_id =
            next_image_content_id.unwrap_or_else(default_next_image_content_id);
        if image_content_by_id.contains_key(&next_image_content_id) {
            return Err("next image content identity collides with retained content".to_owned());
        }
        if next_image_content_id.get_raw_image_content_id() == u64::MAX {
            return Err("image content identity space is exhausted".to_owned());
        }
        Ok(Self {
            image_content_by_id,
            legacy_kitty_content_by_image_id: BTreeMap::new(),
            referenced_image_content_ids: HashSet::new(),
            raster_by_key: HashMap::new(),
            decoded_image_pointers,
            storage_byte_count,
            next_image_content_id,
            is_new_format,
        })
    }

    fn get_or_create_image_content(
        &mut self,
        serialized_image_record: &SerializedImageRecord,
        image_content_id: Option<ImageContentId>,
    ) -> Result<Arc<ImageContent>, String> {
        if self.is_new_format != image_content_id.is_some() {
            return Err("image records must use the content table consistently".to_owned());
        }
        if image_content_id.is_some() && serialized_image_record.decoded_image.is_some() {
            return Err("content-table image records cannot carry inline pixels".to_owned());
        }
        if let Some(image_content_id) = image_content_id {
            let image_content = self
                .image_content_by_id
                .get(&image_content_id)
                .cloned()
                .ok_or_else(|| "image placement refers to missing content".to_owned())?;
            self.referenced_image_content_ids.insert(image_content_id);
            return Ok(image_content);
        }
        let decoded_image = serialized_image_record
            .decoded_image
            .as_ref()
            .ok_or_else(|| "legacy image record is missing pixels".to_owned())?;
        if let Some(image_id) = get_kitty_image_id_from_record(serialized_image_record) {
            if let Some(image_content) = self.legacy_kitty_content_by_image_id.get(&image_id) {
                if image_content.decoded_image.as_ref() != decoded_image.as_ref() {
                    return Err("one Kitty image id refers to different pixels".to_owned());
                }
                return Ok(Arc::clone(image_content));
            }
        }
        let image_content_id = self.allocate_image_content_id()?;
        self.retain_decoded_image(Arc::clone(decoded_image))?;
        let image_content = Arc::new(ImageContent {
            image_content_id,
            decoded_image: Arc::clone(decoded_image),
            animation: serialized_image_record.animation.clone(),
            animation_frame_index: 0,
            animation_loop_count: 0,
            animation_elapsed_nanos: 0,
            is_animation_running: serialized_image_record.animation.is_some(),
            is_animation_loading: false,
            sixel: None,
        });
        if let Some(animation) = &serialized_image_record.animation {
            for animation_frame in animation.list_frames() {
                self.retain_decoded_image(animation_frame.clone_decoded_image())?;
            }
        }
        if let Some(image_id) = get_kitty_image_id_from_record(serialized_image_record) {
            self.legacy_kitty_content_by_image_id
                .insert(image_id, Arc::clone(&image_content));
        }
        Ok(image_content)
    }

    fn allocate_image_content_id(&mut self) -> Result<ImageContentId, String> {
        let mut candidate_image_content_id = self.next_image_content_id;
        loop {
            let next_image_content_id = candidate_image_content_id
                .get_raw_image_content_id()
                .checked_add(1)
                .and_then(ImageContentId::from_raw_image_content_id)
                .ok_or_else(|| "image content identity space is exhausted".to_owned())?;
            if !self
                .image_content_by_id
                .contains_key(&candidate_image_content_id)
                && !self
                    .legacy_kitty_content_by_image_id
                    .values()
                    .any(|legacy_image_content| {
                        legacy_image_content.image_content_id == candidate_image_content_id
                    })
            {
                self.next_image_content_id = next_image_content_id;
                return Ok(candidate_image_content_id);
            }
            candidate_image_content_id = next_image_content_id;
        }
    }

    fn add_storage_byte_count(&mut self, requested_byte_count: usize) -> Result<(), String> {
        self.ensure_storage_byte_count(requested_byte_count)?;
        self.storage_byte_count += requested_byte_count;
        Ok(())
    }

    fn ensure_storage_byte_count(&self, requested_byte_count: usize) -> Result<(), String> {
        let used_byte_count = self.storage_byte_count;
        let total_byte_count = used_byte_count
            .checked_add(requested_byte_count)
            .ok_or_else(|| {
                ImagePlacementError::StorageLimit {
                    used_byte_count,
                    requested_byte_count,
                    byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
                }
                .to_string()
            })?;
        if total_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT {
            return Err(ImagePlacementError::StorageLimit {
                used_byte_count,
                requested_byte_count,
                byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
            }
            .to_string());
        }
        Ok(())
    }

    fn cache_raster_image(
        &mut self,
        key: RasterCacheKey,
        raster: Arc<crate::graphics::DecodedImage>,
    ) -> Result<Arc<crate::graphics::DecodedImage>, String> {
        if let Some(existing_raster) = self.raster_by_key.get(&key) {
            return Ok(Arc::clone(existing_raster));
        }
        if self.raster_by_key.len() == MAX_IMAGE_PLACEMENT_COUNT {
            return Err(ImagePlacementError::TooManyPlacements {
                placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            }
            .to_string());
        }
        self.retain_decoded_image(Arc::clone(&raster))?;
        self.raster_by_key.insert(key, Arc::clone(&raster));
        Ok(raster)
    }

    fn retain_raster_image(
        &mut self,
        raster: Arc<crate::graphics::DecodedImage>,
    ) -> Result<Arc<crate::graphics::DecodedImage>, String> {
        self.retain_decoded_image(Arc::clone(&raster))?;
        Ok(raster)
    }

    fn retain_decoded_image(
        &mut self,
        decoded_image: Arc<crate::graphics::DecodedImage>,
    ) -> Result<(), String> {
        if self
            .decoded_image_pointers
            .insert(Arc::as_ptr(&decoded_image))
        {
            self.add_storage_byte_count(decoded_image.rgba_bytes.len())?;
        }
        Ok(())
    }

    fn finish_restore(self) -> Result<ImageContentId, String> {
        if self.is_new_format
            && self.referenced_image_content_ids.len() != self.image_content_by_id.len()
        {
            return Err("image content table contains unreferenced entries".to_owned());
        }
        Ok(self.next_image_content_id)
    }
}

fn validate_animation_content_state(
    decoded_image: &Arc<crate::graphics::DecodedImage>,
    animation: Option<&Arc<DecodedAnimation>>,
    animation_frame_index: u32,
    animation_loop_count: u32,
    animation_elapsed_nanos: u64,
    is_animation_running: bool,
    is_animation_loading: bool,
) -> Result<(), String> {
    let Some(animation) = animation else {
        if animation_frame_index != 0
            || animation_loop_count != 0
            || animation_elapsed_nanos != 0
            || is_animation_running
            || is_animation_loading
        {
            return Err("image content has playback state without animation frames".to_owned());
        }
        return Ok(());
    };
    let animation_frame_index_usize = usize::try_from(animation_frame_index).map_err(|_| {
        ImagePlacementError::AnimationFrameNotFound {
            frame_index: animation_frame_index,
        }
        .to_string()
    })?;
    let animation_frame_image = animation
        .list_frames()
        .get(animation_frame_index_usize)
        .ok_or_else(|| {
            ImagePlacementError::AnimationFrameNotFound {
                frame_index: animation_frame_index,
            }
            .to_string()
        })?
        .get_decoded_image();
    if animation_frame_image != decoded_image.as_ref() {
        return Err("image content pixels do not match its current animation frame".to_owned());
    }
    if animation
        .get_loop_policy()
        .get_total_playback_count()
        .is_some_and(|total| animation_loop_count > total)
    {
        return Err("image content playback count exceeds its animation loop policy".to_owned());
    }
    if is_animation_loading && !is_animation_running {
        return Err("image content cannot load an animation while stopped".to_owned());
    }
    Ok(())
}

fn get_kitty_image_id_from_record(serialized_image_record: &SerializedImageRecord) -> Option<u32> {
    (serialized_image_record.protocol == GraphicsProtocol::Kitty)
        .then_some(serialized_image_record.display.image_id)
        .flatten()
        .filter(|image_id| *image_id != 0)
}

fn restore_raster_plan(
    image_record: &ImageRecord,
    serialized_placement: &SerializedImagePlacement,
    image_content: &Arc<ImageContent>,
    restore_builder: &mut RestoreImageBuilder,
) -> Result<(RasterPlan, Option<Arc<crate::graphics::DecodedImage>>), String> {
    if let Some(raster_plan) = &serialized_placement.plan {
        validate_raster_plan(
            image_record,
            raster_plan,
            serialized_placement.column_count,
            serialized_placement.row_count,
        )?;
        let raster = match &serialized_placement.raster {
            Some(raster) => {
                if (raster.pixel_width, raster.pixel_height) != raster_plan.canvas_size {
                    return Err("image raster dimensions do not match its raster plan".to_owned());
                }
                Some(restore_builder.retain_raster_image(raster.clone())?)
            }
            None => {
                let raster_cache_key = (
                    image_content.image_content_id,
                    raster_plan.source_rect,
                    raster_plan.target_size,
                    raster_plan.canvas_size,
                    raster_plan.pixel_offset,
                );
                if let Some(cached_raster) = restore_builder.raster_by_key.get(&raster_cache_key) {
                    Some(Arc::clone(cached_raster))
                } else {
                    let is_identity = raster_plan.target_size == raster_plan.canvas_size
                        && raster_plan.pixel_offset == (0, 0)
                        && raster_plan.source_rect
                            == image_record
                                .compute_source_rect()
                                .map_err(|source_rect_error| source_rect_error.to_string())?
                        && raster_plan.target_size
                            == (
                                image_record.image.pixel_width,
                                image_record.image.pixel_height,
                            );
                    if !is_identity {
                        let raster_byte_count = usize::try_from(raster_plan.canvas_size.0)
                            .ok()
                            .and_then(|image_pixel_width| {
                                usize::try_from(raster_plan.canvas_size.1).ok().and_then(
                                    |image_pixel_height| {
                                        image_pixel_width
                                            .checked_mul(image_pixel_height)
                                            .and_then(|pixel_count| pixel_count.checked_mul(4))
                                    },
                                )
                            })
                            .ok_or_else(|| "image raster plan byte count overflows".to_owned())?;
                        restore_builder.ensure_storage_byte_count(raster_byte_count)?;
                    }
                    let raster = raster::rebuild_raster_image(image_record, raster_plan)
                        .map_err(|raster_error| raster_error.to_string())?;
                    raster
                        .map(|raster| restore_builder.cache_raster_image(raster_cache_key, raster))
                        .transpose()?
                }
            }
        };
        return Ok((raster_plan.clone(), raster));
    }
    let image_geometry = serialized_placement.geometry.unwrap_or(ImageCellGeometry {
        full_size: Size {
            column_count: serialized_placement.column_count,
            row_count: serialized_placement.row_count,
        },
        cell_offset: Point { column: 0, row: 0 },
    });
    validate_image_geometry(
        image_record,
        serialized_placement.column_count,
        serialized_placement.row_count,
        Some(image_geometry),
    )?;
    let raster = serialized_placement
        .raster
        .clone()
        .map(|raster| restore_builder.retain_raster_image(raster))
        .transpose()?;
    Ok((
        legacy_image_raster_plan(image_record, image_geometry, raster.as_ref()),
        raster,
    ))
}

fn validate_new_format_placement(
    serialized_placement: &SerializedImagePlacement,
    restore_builder: &RestoreImageBuilder,
) -> Result<(), String> {
    if !restore_builder.is_new_format {
        return Ok(());
    }
    if serialized_placement.plan.is_none() {
        return Err("content-table image placements require a raster plan".to_owned());
    }
    if serialized_placement.geometry.is_some() || serialized_placement.raster.is_some() {
        return Err("content-table image placements cannot carry legacy raster fields".to_owned());
    }
    Ok(())
}

fn validate_raster_plan(
    image_record: &ImageRecord,
    raster_plan: &RasterPlan,
    column_count: u16,
    row_count: u16,
) -> Result<(), String> {
    validate_image_pixels(&image_record.image)?;
    if !raster_plan.geometry.is_visible_size_contained(Size {
        column_count,
        row_count,
    }) {
        return Err("image clipping exceeds its complete cell dimensions".to_owned());
    }
    let source_rect = image_record
        .compute_source_rect()
        .map_err(|source_rect_error| source_rect_error.to_string())?;
    if source_rect != raster_plan.source_rect {
        return Err("image raster plan source does not match its image record".to_owned());
    }
    if raster_plan.target_size.0 == 0
        || raster_plan.target_size.1 == 0
        || raster_plan.canvas_size.0 == 0
        || raster_plan.canvas_size.1 == 0
        || u64::from(raster_plan.pixel_offset.0) + u64::from(raster_plan.target_size.0)
            > u64::from(raster_plan.canvas_size.0)
        || u64::from(raster_plan.pixel_offset.1) + u64::from(raster_plan.target_size.1)
            > u64::from(raster_plan.canvas_size.1)
    {
        return Err("image raster plan dimensions are invalid".to_owned());
    }
    let source_end_x = raster_plan
        .source_rect
        .0
        .checked_add(raster_plan.source_rect.2);
    let source_end_y = raster_plan
        .source_rect
        .1
        .checked_add(raster_plan.source_rect.3);
    if source_end_x.is_none_or(|end| end > image_record.image.pixel_width)
        || source_end_y.is_none_or(|end| end > image_record.image.pixel_height)
        || raster_plan.source_rect.2 == 0
        || raster_plan.source_rect.3 == 0
    {
        return Err("image raster plan source overflows".to_owned());
    }
    let raster_byte_count = usize::try_from(raster_plan.canvas_size.0)
        .ok()
        .and_then(|width| {
            usize::try_from(raster_plan.canvas_size.1)
                .ok()
                .and_then(|height| {
                    width
                        .checked_mul(height)
                        .and_then(|pixels| pixels.checked_mul(4))
                })
        })
        .ok_or_else(|| "image raster plan byte count overflows".to_owned())?;
    if raster_plan.canvas_size.0 > MAX_IMAGE_SIDE_PIXEL_COUNT as u32
        || raster_plan.canvas_size.1 > MAX_IMAGE_SIDE_PIXEL_COUNT as u32
        || raster_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT
    {
        return Err("image raster plan exceeds image limits".to_owned());
    }
    Ok(())
}

fn restore_live_image_placement(
    serialized_placement: SerializedImagePlacement,
    restore_builder: &mut RestoreImageBuilder,
) -> Result<ImagePlacement, String> {
    validate_new_format_placement(&serialized_placement, restore_builder)?;
    let image_placement_id = serialized_placement.image_placement_id;
    if image_placement_id == 0 {
        return Err("image placement identity must be nonzero".to_owned());
    }
    if serialized_placement.column_count == 0 || serialized_placement.row_count == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    if serialized_placement.anchor.0 > u64::from(u16::MAX) {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    let image_content = restore_builder.get_or_create_image_content(
        &serialized_placement.image_record,
        serialized_placement.image_content_id,
    )?;
    let image_record = serialized_placement
        .image_record
        .clone()
        .into_image_record(Arc::clone(&image_content))?;
    if !matches!(
        image_record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    let (raster_plan, raster_image) = restore_raster_plan(
        &image_record,
        &serialized_placement,
        &image_content,
        restore_builder,
    )?;
    let end_row = serialized_placement
        .anchor
        .0
        .checked_add(u64::from(serialized_placement.row_count))
        .ok_or_else(|| "image placement row extent overflows u64".to_owned())?;
    let end_column =
        u64::from(serialized_placement.anchor.1) + u64::from(serialized_placement.column_count);
    if end_row > u64::from(u16::MAX) + 1 || end_column > u64::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    Ok(ImagePlacement {
        image_placement_id,
        image_record,
        image_content,
        anchor: (
            serialized_placement.anchor.0 as u16,
            serialized_placement.anchor.1,
        ),
        column_count: serialized_placement.column_count,
        row_count: serialized_placement.row_count,
        plan: raster_plan,
        raster: raster_image,
    })
}

fn restore_history_image_placement(
    serialized_placement: SerializedImagePlacement,
    restore_builder: &mut RestoreImageBuilder,
) -> Result<PrimaryHistoryImagePlacement, String> {
    validate_new_format_placement(&serialized_placement, restore_builder)?;
    if serialized_placement.image_placement_id == 0 {
        return Err("image placement identity must be nonzero".to_owned());
    }
    if serialized_placement.column_count == 0 || serialized_placement.row_count == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    let image_content = restore_builder.get_or_create_image_content(
        &serialized_placement.image_record,
        serialized_placement.image_content_id,
    )?;
    let image_record = serialized_placement
        .image_record
        .clone()
        .into_image_record(Arc::clone(&image_content))?;
    if !matches!(
        image_record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    let (raster_plan, raster_image) = restore_raster_plan(
        &image_record,
        &serialized_placement,
        &image_content,
        restore_builder,
    )?;
    serialized_placement
        .anchor
        .0
        .checked_add(u64::from(serialized_placement.row_count))
        .ok_or_else(|| "image placement row extent overflows u64".to_owned())?;
    let end_column =
        u64::from(serialized_placement.anchor.1) + u64::from(serialized_placement.column_count);
    if end_column > u64::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    Ok(PrimaryHistoryImagePlacement {
        image_placement_id: serialized_placement.image_placement_id,
        image_record,
        image_content,
        anchor: serialized_placement.anchor,
        column_count: serialized_placement.column_count,
        row_count: serialized_placement.row_count,
        plan: raster_plan,
        raster: raster_image,
    })
}

pub(super) fn restore_serialized_image_state(
    primary_image_placements: Vec<SerializedImagePlacement>,
    primary_image_history: Vec<SerializedImagePlacement>,
    alternate_image_placements: Vec<SerializedImagePlacement>,
    serialized_kitty_images: Vec<SerializedKittyImage>,
    serialized_image_contents: Option<Vec<SerializedImageContent>>,
    next_image_content_id: Option<ImageContentId>,
) -> Result<RestoredImageState, String> {
    let total_placement_count = primary_image_placements
        .len()
        .checked_add(primary_image_history.len())
        .and_then(|placement_count| placement_count.checked_add(alternate_image_placements.len()))
        .ok_or_else(|| "image placement count overflows".to_owned())?;
    if total_placement_count > MAX_IMAGE_PLACEMENT_COUNT {
        return Err(ImagePlacementError::TooManyPlacements {
            placement_count: total_placement_count,
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        }
        .to_string());
    }
    if serialized_kitty_images.len() > MAX_IMAGE_PLACEMENT_COUNT {
        return Err(ImagePlacementError::TooManyPlacements {
            placement_count: serialized_kitty_images.len(),
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        }
        .to_string());
    }
    let mut restore_builder = RestoreImageBuilder::from_serialized_image_contents(
        serialized_image_contents,
        next_image_content_id,
    )?;
    let mut primary_image_placement_ids = HashSet::new();
    if primary_image_placements
        .iter()
        .any(|placement| !primary_image_placement_ids.insert(placement.image_placement_id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let mut history_image_placement_ids = HashSet::new();
    if primary_image_history
        .iter()
        .any(|placement| !history_image_placement_ids.insert(placement.image_placement_id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let mut alternate_image_placement_ids = HashSet::new();
    if alternate_image_placements
        .iter()
        .any(|placement| !alternate_image_placement_ids.insert(placement.image_placement_id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let primary_image_placements = primary_image_placements
        .into_iter()
        .map(|serialized_placement| {
            restore_live_image_placement(serialized_placement, &mut restore_builder)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let primary_image_history = primary_image_history
        .into_iter()
        .map(|serialized_placement| {
            restore_history_image_placement(serialized_placement, &mut restore_builder)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let alternate_image_placements = alternate_image_placements
        .into_iter()
        .map(|serialized_placement| {
            restore_live_image_placement(serialized_placement, &mut restore_builder)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut kitty_images = Vec::with_capacity(serialized_kitty_images.len());
    let mut kitty_image_identities = HashSet::new();
    for serialized_kitty_image in serialized_kitty_images {
        if serialized_kitty_image.image_record.action == ImageAction::Display
            && !serialized_kitty_image.is_virtual_placement
        {
            return Err("a retained Kitty upload requires a transmit action".to_owned());
        }
        if serialized_kitty_image.is_virtual_placement
            && (!serialized_kitty_image
                .image_record
                .display
                .is_unicode_placeholder
                || serialized_kitty_image.virtual_screen.is_none())
        {
            return Err("a virtual Kitty placement has invalid metadata".to_owned());
        }
        let kitty_image_id =
            get_kitty_image_id_from_record(&serialized_kitty_image.image_record)
                .ok_or_else(|| "a retained Kitty upload requires a nonzero image id".to_owned())?;
        let kitty_image_identity = if serialized_kitty_image.is_virtual_placement {
            (
                kitty_image_id,
                serialized_kitty_image.image_record.display.placement_id,
                true,
                serialized_kitty_image.virtual_screen == Some(Screen::Alternate),
            )
        } else {
            (kitty_image_id, None, false, false)
        };
        if !kitty_image_identities.insert(kitty_image_identity) {
            return Err("retained Kitty image ids must be unique".to_owned());
        }
        let image_content = restore_builder.get_or_create_image_content(
            &serialized_kitty_image.image_record,
            serialized_kitty_image.image_content_id,
        )?;
        let image_record = serialized_kitty_image
            .image_record
            .clone()
            .into_image_record(Arc::clone(&image_content))?;
        kitty_images.push(KittyImage {
            image_record,
            image_content,
            is_virtual_placement: serialized_kitty_image.is_virtual_placement,
            virtual_screen: serialized_kitty_image.virtual_screen,
        });
    }
    let mut retained_kitty_ids = kitty_images
        .iter()
        .filter_map(|image| get_kitty_image_id(image.image_record.as_ref()))
        .collect::<HashSet<_>>();
    for placement in primary_image_placements
        .iter()
        .chain(&alternate_image_placements)
    {
        let Some(kitty_image_id) = get_kitty_image_id(placement.image_record.as_ref()) else {
            continue;
        };
        if retained_kitty_ids.insert(kitty_image_id) {
            let mut upload = placement.image_record.as_ref().clone();
            upload.action = ImageAction::Transmit;
            upload.display.image_id = Some(kitty_image_id);
            kitty_images.push(KittyImage {
                image_record: Arc::new(upload),
                image_content: Arc::clone(&placement.image_content),
                is_virtual_placement: false,
                virtual_screen: None,
            });
        }
    }
    for placement in &primary_image_history {
        let Some(kitty_image_id) = get_kitty_image_id(placement.image_record.as_ref()) else {
            continue;
        };
        if retained_kitty_ids.insert(kitty_image_id) {
            let mut upload = placement.image_record.as_ref().clone();
            upload.action = ImageAction::Transmit;
            upload.display.image_id = Some(kitty_image_id);
            kitty_images.push(KittyImage {
                image_record: Arc::new(upload),
                image_content: Arc::clone(&placement.image_content),
                is_virtual_placement: false,
                virtual_screen: None,
            });
        }
    }
    let next_image_content_id = restore_builder.finish_restore()?;
    Ok(RestoredImageState {
        primary_image_placements,
        primary_image_history,
        alternate_image_placements,
        kitty_images,
        next_image_content_id,
    })
}

/// One displayed image and the cells covered by its placement rectangle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImagePlacement {
    /// The terminal-local identity for this placement.
    image_placement_id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    image_record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    #[serde(skip)]
    image_content: Arc<ImageContent>,
    /// The zero-based row and column of the upper-left covered cell.
    anchor: (u16, u16),
    /// The number of covered columns.
    column_count: u16,
    /// The number of covered rows.
    row_count: u16,
    /// The transform and complete cell geometry used to rebuild the raster.
    plan: RasterPlan,
    /// Pixel-sized output padded to the complete cell rectangle.
    raster: Option<Arc<crate::graphics::DecodedImage>>,
}

/// An image placement whose primary-screen row is addressed in the retained
/// history and live-screen row space.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct PrimaryHistoryImagePlacement {
    /// The terminal-local identity for this placement.
    image_placement_id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    image_record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    #[serde(skip)]
    image_content: Arc<ImageContent>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    column_count: u16,
    /// The number of covered rows.
    row_count: u16,
    /// The transform and complete cell geometry used to rebuild the raster.
    plan: RasterPlan,
    /// Pixel-sized output padded to the complete cell rectangle.
    raster: Option<Arc<crate::graphics::DecodedImage>>,
}

/// One placement addressed by the absolute primary row space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AbsoluteImagePlacement {
    /// The terminal-local identity for this placement.
    image_placement_id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    image_record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    image_content: Arc<ImageContent>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    column_count: u16,
    /// The number of covered rows.
    row_count: u16,
    /// The transform and complete cell geometry used to rebuild the raster.
    plan: RasterPlan,
    /// Pixel-sized output padded to the complete cell rectangle.
    raster: Option<Arc<crate::graphics::DecodedImage>>,
}

#[derive(Debug, Clone, Copy)]
enum ActiveImagePlacementSlot {
    Live(usize),
    History(usize),
}

impl ImagePlacement {
    fn from_image_record(
        image_placement_id: ImagePlacementId,
        image_record: Arc<ImageRecord>,
        image_content: Arc<ImageContent>,
        plan: RasterPlan,
        column_count: u16,
        row_count: u16,
        raster: Option<Arc<crate::graphics::DecodedImage>>,
    ) -> Self {
        ImagePlacement {
            image_placement_id,
            image_record,
            image_content,
            anchor: (0, 0),
            column_count,
            row_count,
            plan,
            raster,
        }
    }

    fn with_image_anchor(&self, anchor: (u16, u16)) -> Self {
        ImagePlacement {
            image_placement_id: self.image_placement_id,
            image_record: Arc::clone(&self.image_record),
            image_content: Arc::clone(&self.image_content),
            anchor,
            column_count: self.column_count,
            row_count: self.row_count,
            plan: self.plan.clone(),
            raster: self.raster.clone(),
        }
    }

    /// Return the terminal-local placement identity.
    #[must_use]
    pub fn get_image_placement_id(&self) -> ImagePlacementId {
        self.image_placement_id
    }

    /// Return the complete image record retained by this placement.
    #[must_use]
    pub fn get_image_record(&self) -> &ImageRecord {
        self.image_record.as_ref()
    }

    /// Return the shared complete image record retained by this placement.
    #[must_use]
    pub fn clone_image_record(&self) -> Arc<ImageRecord> {
        Arc::clone(&self.image_record)
    }

    /// Return the canonical source identity used by this placement.
    #[must_use]
    pub fn get_image_content_id(&self) -> u64 {
        self.image_content
            .image_content_id
            .get_raw_image_content_id()
    }

    /// Return the image pixels prepared for the complete cell rectangle.
    #[must_use]
    pub fn create_render_image_record(&self) -> Arc<ImageRecord> {
        let Some(raster) = &self.raster else {
            return self.clone_image_record();
        };
        let mut image_record = self.image_record.as_ref().clone();
        image_record.image = Arc::clone(raster);
        image_record.display.requested_width = None;
        image_record.display.requested_height = None;
        image_record.display.source_pixel_offset_x = None;
        image_record.display.source_pixel_offset_y = None;
        image_record.display.cell_pixel_offset_x = None;
        image_record.display.cell_pixel_offset_y = None;
        Arc::new(image_record)
    }

    /// Return the zero-based row and column of the placement anchor.
    #[must_use]
    pub fn get_image_anchor(&self) -> (u16, u16) {
        self.anchor
    }

    /// Return the complete image size and the clipped top and left cells.
    #[must_use]
    pub fn get_image_geometry(&self) -> ImageCellGeometry {
        self.plan.geometry
    }

    /// Return the placement dimensions as `(rows, columns)`.
    #[must_use]
    pub fn get_image_cell_dimensions(&self) -> (u16, u16) {
        (self.row_count, self.column_count)
    }

    /// Return whether (`row_index`, `column_index`) is one of the cells covered by this
    /// placement.
    #[must_use]
    pub fn is_cell_covered(&self, row_index: u16, column_index: u16) -> bool {
        u32::from(row_index) >= u32::from(self.anchor.0)
            && u32::from(column_index) >= u32::from(self.anchor.1)
            && u32::from(row_index) < u32::from(self.anchor.0) + u32::from(self.row_count)
            && u32::from(column_index) < u32::from(self.anchor.1) + u32::from(self.column_count)
    }

    /// Visit covered cells in row-major order.
    pub fn list_covered_cells(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        let (anchor_row, anchor_column) = self.anchor;
        (0..self.row_count).flat_map(move |row_offset| {
            (0..self.column_count).map(move |column_offset| {
                (
                    anchor_row
                        .checked_add(row_offset)
                        .expect("validated image placement row fits in u16"),
                    anchor_column
                        .checked_add(column_offset)
                        .expect("validated image placement column fits in u16"),
                )
            })
        })
    }
}

impl PrimaryHistoryImagePlacement {
    fn from_absolute_image_placement(placement: AbsoluteImagePlacement) -> Self {
        PrimaryHistoryImagePlacement {
            image_placement_id: placement.image_placement_id,
            image_record: placement.image_record,
            image_content: placement.image_content,
            anchor: placement.anchor,
            column_count: placement.column_count,
            row_count: placement.row_count,
            plan: placement.plan,
            raster: placement.raster,
        }
    }

    fn into_absolute_image_placement(self) -> AbsoluteImagePlacement {
        AbsoluteImagePlacement {
            image_placement_id: self.image_placement_id,
            image_record: self.image_record,
            image_content: self.image_content,
            anchor: self.anchor,
            column_count: self.column_count,
            row_count: self.row_count,
            plan: self.plan,
            raster: self.raster,
        }
    }
}

impl AbsoluteImagePlacement {
    fn from_live_image_placement(placement: ImagePlacement, live_top_row: u64) -> Option<Self> {
        Some(AbsoluteImagePlacement {
            image_placement_id: placement.image_placement_id,
            image_record: placement.image_record,
            image_content: placement.image_content,
            anchor: (
                live_top_row.checked_add(u64::from(placement.anchor.0))?,
                placement.anchor.1,
            ),
            column_count: placement.column_count,
            row_count: placement.row_count,
            plan: placement.plan,
            raster: placement.raster,
        })
    }

    fn into_live_image_placement(self, live_top_row: u64) -> Option<ImagePlacement> {
        Some(ImagePlacement {
            image_placement_id: self.image_placement_id,
            image_record: self.image_record,
            image_content: self.image_content,
            anchor: (
                u16::try_from(self.anchor.0.checked_sub(live_top_row)?).ok()?,
                self.anchor.1,
            ),
            column_count: self.column_count,
            row_count: self.row_count,
            plan: self.plan,
            raster: self.raster,
        })
    }

    fn clip_to_visible_area(
        mut self,
        top_row: u64,
        end_row_limit: u64,
        column_count: u16,
    ) -> Option<Self> {
        let start_row = self.anchor.0.max(top_row);
        let end_row = self
            .anchor
            .0
            .checked_add(u64::from(self.row_count))?
            .min(end_row_limit);
        let end_column =
            (u32::from(self.anchor.1) + u32::from(self.column_count)).min(u32::from(column_count));
        if start_row >= end_row || end_column <= u32::from(self.anchor.1) {
            return None;
        }
        let removed_row_count = u16::try_from(start_row - self.anchor.0).ok()?;
        self.plan.geometry.cell_offset.row = self
            .plan
            .geometry
            .cell_offset
            .row
            .checked_add(removed_row_count)?;
        self.anchor.0 = start_row;
        self.row_count = u16::try_from(end_row - start_row).ok()?;
        self.column_count = u16::try_from(end_column - u32::from(self.anchor.1)).ok()?;
        Some(self)
    }

    fn with_image_anchor(mut self, anchor: (u64, u16)) -> Self {
        self.anchor = anchor;
        self
    }
}

impl<'de> serde::Deserialize<'de> for ImagePlacement {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ImagePlacementFields {
            image_placement_id: ImagePlacementId,
            image_record: Arc<ImageRecord>,
            anchor: (u16, u16),
            column_count: u16,
            row_count: u16,
            #[serde(default)]
            geometry: Option<ImageCellGeometry>,
            #[serde(default)]
            plan: Option<RasterPlan>,
            #[serde(default)]
            raster: Option<Arc<crate::graphics::DecodedImage>>,
        }

        let image_placement_fields =
            <ImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        if image_placement_fields.image_placement_id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }
        if !matches!(
            image_placement_fields.image_record.action,
            ImageAction::Display | ImageAction::TransmitAndDisplay
        ) {
            return Err(serde::de::Error::custom(
                "image placement image record must be a display record",
            ));
        }
        if image_placement_fields.column_count == 0 || image_placement_fields.row_count == 0 {
            return Err(serde::de::Error::custom(
                "image placement dimensions must be nonzero",
            ));
        }
        validate_image_geometry(
            &image_placement_fields.image_record,
            image_placement_fields.column_count,
            image_placement_fields.row_count,
            image_placement_fields
                .plan
                .as_ref()
                .map(|plan| plan.geometry)
                .or(image_placement_fields.geometry),
        )
        .map_err(serde::de::Error::custom)?;
        let row_end = u32::from(image_placement_fields.anchor.0)
            + u32::from(image_placement_fields.row_count);
        let column_end = u32::from(image_placement_fields.anchor.1)
            + u32::from(image_placement_fields.column_count);
        if row_end > u32::from(u16::MAX) + 1 || column_end > u32::from(u16::MAX) + 1 {
            return Err(serde::de::Error::custom(
                "image placement coordinate extent does not fit in u16",
            ));
        }

        let image_record = image_placement_fields.image_record;
        let image_content = legacy_image_content_for_record(&image_record);
        let plan = image_placement_fields.plan.unwrap_or_else(|| {
            legacy_image_raster_plan(
                &image_record,
                image_placement_fields
                    .geometry
                    .unwrap_or(ImageCellGeometry {
                        full_size: Size {
                            column_count: image_placement_fields.column_count,
                            row_count: image_placement_fields.row_count,
                        },
                        cell_offset: Point { column: 0, row: 0 },
                    }),
                image_placement_fields.raster.as_ref(),
            )
        });
        Ok(ImagePlacement {
            image_placement_id: image_placement_fields.image_placement_id,
            image_record,
            anchor: image_placement_fields.anchor,
            column_count: image_placement_fields.column_count,
            row_count: image_placement_fields.row_count,
            plan,
            image_content,
            raster: image_placement_fields.raster,
        })
    }
}

impl<'de> serde::Deserialize<'de> for PrimaryHistoryImagePlacement {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct PrimaryHistoryImagePlacementFields {
            image_placement_id: ImagePlacementId,
            image_record: Arc<ImageRecord>,
            anchor: (u64, u16),
            column_count: u16,
            row_count: u16,
            #[serde(default)]
            geometry: Option<ImageCellGeometry>,
            #[serde(default)]
            plan: Option<RasterPlan>,
            #[serde(default)]
            raster: Option<Arc<crate::graphics::DecodedImage>>,
        }

        let image_placement_fields =
            <PrimaryHistoryImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        validate_history_image_placement(
            &image_placement_fields.image_record,
            image_placement_fields.anchor,
            image_placement_fields.column_count,
            image_placement_fields.row_count,
            image_placement_fields
                .plan
                .as_ref()
                .map(|plan| plan.geometry)
                .or(image_placement_fields.geometry),
        )
        .map_err(serde::de::Error::custom)?;
        if image_placement_fields.image_placement_id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }

        let image_record = image_placement_fields.image_record;
        let image_content = legacy_image_content_for_record(&image_record);
        let plan = image_placement_fields.plan.unwrap_or_else(|| {
            legacy_image_raster_plan(
                &image_record,
                image_placement_fields
                    .geometry
                    .unwrap_or(ImageCellGeometry {
                        full_size: Size {
                            column_count: image_placement_fields.column_count,
                            row_count: image_placement_fields.row_count,
                        },
                        cell_offset: Point { column: 0, row: 0 },
                    }),
                image_placement_fields.raster.as_ref(),
            )
        });
        Ok(PrimaryHistoryImagePlacement {
            image_placement_id: image_placement_fields.image_placement_id,
            image_record,
            anchor: image_placement_fields.anchor,
            column_count: image_placement_fields.column_count,
            row_count: image_placement_fields.row_count,
            plan,
            image_content,
            raster: image_placement_fields.raster,
        })
    }
}

fn validate_history_image_placement(
    image_record: &ImageRecord,
    placement_anchor: (u64, u16),
    column_count: u16,
    row_count: u16,
    image_geometry: Option<ImageCellGeometry>,
) -> Result<(), String> {
    if !matches!(
        image_record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_string());
    }
    if column_count == 0 || row_count == 0 {
        return Err("image placement dimensions must be nonzero".to_string());
    }
    validate_image_geometry(image_record, column_count, row_count, image_geometry)?;
    placement_anchor
        .0
        .checked_add(u64::from(row_count))
        .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
    let column_end = u32::from(placement_anchor.1) + u32::from(column_count);
    if column_end > u32::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_string());
    }
    Ok(())
}

fn validate_image_geometry(
    image_record: &ImageRecord,
    column_count: u16,
    row_count: u16,
    image_geometry: Option<ImageCellGeometry>,
) -> Result<(), String> {
    image_record
        .compute_source_rect()
        .map_err(|source_rect_error| source_rect_error.to_string())?;
    if let Some(image_geometry) = image_geometry {
        if !image_geometry.is_visible_size_contained(Size {
            column_count,
            row_count,
        }) {
            return Err("image clipping exceeds its complete cell dimensions".to_string());
        }
    } else {
        let (image_column_count, image_row_count) = compute_image_cell_dimensions(image_record)
            .map_err(|dimension_error| dimension_error.to_string())?;
        if image_column_count != u32::from(column_count) || image_row_count != u32::from(row_count)
        {
            return Err("image placement dimensions do not match its image record".to_string());
        }
    }
    Ok(())
}

fn legacy_image_raster_plan(
    image_record: &ImageRecord,
    image_geometry: ImageCellGeometry,
    raster_image: Option<&Arc<crate::graphics::DecodedImage>>,
) -> RasterPlan {
    let source_rect = image_record.compute_source_rect().unwrap_or((
        0,
        0,
        image_record.image.pixel_width,
        image_record.image.pixel_height,
    ));
    let canvas_size = raster_image.map_or(
        (
            image_record.image.pixel_width,
            image_record.image.pixel_height,
        ),
        |decoded_image| (decoded_image.pixel_width, decoded_image.pixel_height),
    );
    RasterPlan {
        geometry: image_geometry,
        source_rect,
        target_size: canvas_size,
        canvas_size,
        pixel_offset: (0, 0),
    }
}

pub(super) fn default_next_image_placement_id() -> ImagePlacementId {
    1
}

pub(super) fn default_next_image_content_id() -> ImageContentId {
    ImageContentId::from_raw_image_content_id(1).expect("one is a valid image content id")
}

struct ImageStorageAccounting {
    used_byte_count: usize,
    decoded_image_pointer_addresses: HashSet<usize>,
}

pub(super) fn validate_image_state(
    terminal_state_fields: &super::TerminalStateFields,
) -> Result<(), String> {
    if terminal_state_fields.next_image_placement_id == 0 {
        return Err("next image placement identity must be nonzero".to_string());
    }
    let total_image_placement_count = terminal_state_fields
        .primary_image_placements
        .len()
        .checked_add(terminal_state_fields.primary_image_history.len())
        .and_then(|count| count.checked_add(terminal_state_fields.alternate_image_placements.len()))
        .ok_or_else(|| "image placement count overflows".to_owned())?;
    if total_image_placement_count > MAX_IMAGE_PLACEMENT_COUNT {
        return Err(ImagePlacementError::TooManyPlacements {
            placement_count: total_image_placement_count,
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        }
        .to_string());
    }
    if terminal_state_fields.kitty_images.len() > MAX_IMAGE_PLACEMENT_COUNT {
        return Err(ImagePlacementError::TooManyPlacements {
            placement_count: terminal_state_fields.kitty_images.len(),
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        }
        .to_string());
    }
    let mut image_placement_ids = HashSet::with_capacity(total_image_placement_count);
    let mut image_content_pointer_by_id = BTreeMap::<ImageContentId, (usize, usize)>::new();
    let mut image_storage_accounting = ImageStorageAccounting {
        used_byte_count: 0,
        decoded_image_pointer_addresses: HashSet::new(),
    };
    let mut retained_kitty_identities = HashSet::new();
    for kitty_image in &terminal_state_fields.kitty_images {
        if kitty_image.image_record.action == ImageAction::Display
            && !kitty_image.is_virtual_placement
        {
            return Err("a retained Kitty upload requires a transmit action".to_owned());
        }
        if kitty_image.is_virtual_placement
            && (!kitty_image.image_record.display.is_unicode_placeholder
                || kitty_image.virtual_screen.is_none())
        {
            return Err("a virtual Kitty placement has invalid metadata".to_owned());
        }
        let kitty_image_id = get_kitty_image_id(kitty_image.image_record.as_ref())
            .ok_or_else(|| "a retained Kitty upload requires a nonzero image id".to_owned())?;
        let kitty_image_identity = (
            kitty_image_id,
            kitty_image.is_virtual_placement,
            kitty_image.virtual_screen == Some(Screen::Alternate),
            if kitty_image.is_virtual_placement {
                kitty_image.display.placement_id
            } else {
                None
            },
        );
        if !retained_kitty_identities.insert(kitty_image_identity) {
            return Err("retained Kitty image ids must be unique".to_owned());
        }
        if !Arc::ptr_eq(
            &kitty_image.image_record.image,
            &kitty_image.image_content.decoded_image,
        ) {
            return Err("Kitty image record is not aliased to its content".to_owned());
        }
        validate_content_storage(
            &kitty_image.image_content,
            &mut image_content_pointer_by_id,
            &mut image_storage_accounting,
        )?;
    }
    let mut primary_kitty_identities = HashSet::new();
    for placement in &terminal_state_fields.primary_image_placements {
        validate_live_image_placement(
            placement,
            if terminal_state_fields.native_image_coverage
                && placement.image_record.protocol != GraphicsProtocol::Kitty
            {
                (u16::MAX, u16::MAX)
            } else {
                terminal_state_fields.primary.get_grid_dimensions()
            },
            &mut image_placement_ids,
            &mut primary_kitty_identities,
            &mut image_content_pointer_by_id,
            &mut image_storage_accounting,
        )?;
    }
    let total_pushed = terminal_state_fields
        .scrollback
        .get_total_pushed_line_count();
    let retained_rows = terminal_state_fields.scrollback.get_retained_line_count() as u64;
    if retained_rows > total_pushed {
        return Err(ImagePlacementError::HistoryRowsExceedCounter {
            retained_row_count: retained_rows,
            total_pushed_row_count: total_pushed,
        }
        .to_string());
    }
    let grid_rows = terminal_state_fields.primary.get_grid_dimensions().0;
    let history_first = total_pushed
        .saturating_sub(terminal_state_fields.scrollback.get_retained_line_count() as u64);
    let live_end = total_pushed
        .checked_add(u64::from(grid_rows))
        .ok_or_else(|| {
            ImagePlacementError::HistoryRangeOverflow {
                total_pushed_row_count: total_pushed,
                grid_rows,
            }
            .to_string()
        })?;
    let grid_columns = terminal_state_fields.primary.get_grid_dimensions().1;
    for placement in &terminal_state_fields.primary_image_history {
        if !matches!(
            placement.image_record.action,
            ImageAction::Display | ImageAction::TransmitAndDisplay
        ) {
            return Err("image placement image record must be a display record".to_owned());
        }
        if placement.column_count == 0 || placement.row_count == 0 {
            return Err("image placement dimensions must be nonzero".to_owned());
        }
        if !image_placement_ids.insert(placement.image_placement_id) {
            return Err("image placement identities must be unique across screens".to_string());
        }
        if let Some(identity) = get_kitty_placement_identity(placement.image_record.as_ref()) {
            if !primary_kitty_identities.insert(identity) {
                return Err(
                    "kitty image and placement identities must be unique per screen".to_string(),
                );
            }
        }
        let history_end = placement
            .anchor
            .0
            .checked_add(u64::from(placement.row_count))
            .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
        if placement.anchor.0 >= total_pushed
            || placement.anchor.0 < history_first
            || history_end > live_end
        {
            return Err(ImagePlacementError::HistoryOutOfBounds {
                anchor_row: placement.anchor.0,
                anchor_column: placement.anchor.1,
                column_count: placement.column_count,
                row_count: placement.row_count,
                first_row: history_first,
                retained_end: live_end,
            }
            .to_string());
        }
        if u32::from(placement.anchor.1) + u32::from(placement.column_count)
            > u32::from(grid_columns)
        {
            return Err(ImagePlacementError::HistoryWidthOutOfBounds {
                anchor_row: placement.anchor.0,
                anchor_column: placement.anchor.1,
                column_count: placement.column_count,
                grid_columns,
            }
            .to_string());
        }
        validate_content_storage(
            &placement.image_content,
            &mut image_content_pointer_by_id,
            &mut image_storage_accounting,
        )?;
        if !Arc::ptr_eq(
            &placement.image_record.image,
            &placement.image_content.decoded_image,
        ) {
            return Err("image placement record is not aliased to its content".to_owned());
        }
        validate_raster_plan(
            placement.image_record.as_ref(),
            &placement.plan,
            placement.column_count,
            placement.row_count,
        )?;
        if let Some(raster) = &placement.raster {
            validate_pixel_storage(raster, &mut image_storage_accounting)?;
        }
    }
    let mut alternate_kitty_identities = HashSet::new();
    for placement in &terminal_state_fields.alternate_image_placements {
        validate_live_image_placement(
            placement,
            if terminal_state_fields.native_image_coverage
                && placement.image_record.protocol != GraphicsProtocol::Kitty
            {
                (u16::MAX, u16::MAX)
            } else {
                terminal_state_fields.alternate.get_grid_dimensions()
            },
            &mut image_placement_ids,
            &mut alternate_kitty_identities,
            &mut image_content_pointer_by_id,
            &mut image_storage_accounting,
        )?;
    }
    if image_content_pointer_by_id.contains_key(&terminal_state_fields.next_image_content_id) {
        return Err("next image content identity collides with retained content".to_owned());
    }
    if terminal_state_fields
        .next_image_content_id
        .get_raw_image_content_id()
        == u64::MAX
    {
        return Err("image content identity space is exhausted".to_owned());
    }
    Ok(())
}

fn validate_live_image_placement(
    placement: &ImagePlacement,
    (grid_row_count, grid_column_count): (u16, u16),
    image_placement_ids: &mut HashSet<ImagePlacementId>,
    kitty_identities: &mut HashSet<(u32, u32)>,
    image_content_pointer_by_id: &mut BTreeMap<ImageContentId, (usize, usize)>,
    image_storage_accounting: &mut ImageStorageAccounting,
) -> Result<(), String> {
    if !matches!(
        placement.image_record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    if placement.column_count == 0 || placement.row_count == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    if !image_placement_ids.insert(placement.image_placement_id) {
        return Err("image placement identities must be unique across screens".to_string());
    }
    if let Some(identity) = get_kitty_placement_identity(placement.get_image_record()) {
        if !kitty_identities.insert(identity) {
            return Err(
                "kitty image and placement identities must be unique per screen".to_string(),
            );
        }
    }
    if !Arc::ptr_eq(
        &placement.image_record.image,
        &placement.image_content.decoded_image,
    ) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    let row_end = u32::from(placement.anchor.0) + u32::from(placement.row_count);
    let column_end = u32::from(placement.anchor.1) + u32::from(placement.column_count);
    if placement.image_record.display.relative_image_id.is_none()
        && (row_end > u32::from(grid_row_count) || column_end > u32::from(grid_column_count))
    {
        return Err(ImagePlacementError::OutOfBounds {
            anchor_row: placement.anchor.0,
            anchor_column: placement.anchor.1,
            column_count: placement.column_count,
            row_count: placement.row_count,
            grid_rows: grid_row_count,
            grid_columns: grid_column_count,
        }
        .to_string());
    }
    validate_content_storage(
        &placement.image_content,
        image_content_pointer_by_id,
        image_storage_accounting,
    )?;
    validate_raster_plan(
        placement.get_image_record(),
        &placement.plan,
        placement.column_count,
        placement.row_count,
    )?;
    if let Some(raster) = &placement.raster {
        validate_pixel_storage(raster, image_storage_accounting)?;
    }
    Ok(())
}

fn validate_content_storage(
    image_content: &Arc<ImageContent>,
    image_content_pointer_by_id: &mut BTreeMap<ImageContentId, (usize, usize)>,
    image_storage_accounting: &mut ImageStorageAccounting,
) -> Result<(), String> {
    let image_content_pointer = (
        Arc::as_ptr(image_content) as usize,
        Arc::as_ptr(&image_content.decoded_image) as usize,
    );
    if let Some(existing_image_content_pointer) =
        image_content_pointer_by_id.get(&image_content.image_content_id)
    {
        if *existing_image_content_pointer != image_content_pointer {
            return Err("one image content identity has different pixels".to_owned());
        }
        return Ok(());
    }
    if image_content_pointer_by_id.len() == MAX_IMAGE_PLACEMENT_COUNT {
        return Err(ImagePlacementError::TooManyPlacements {
            placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        }
        .to_string());
    }
    image_content_pointer_by_id.insert(image_content.image_content_id, image_content_pointer);
    validate_image_pixels(&image_content.decoded_image)?;
    if image_storage_accounting
        .decoded_image_pointer_addresses
        .insert(Arc::as_ptr(&image_content.decoded_image) as usize)
    {
        add_image_storage_accounting_byte_count(
            image_content.decoded_image.rgba_bytes.len(),
            image_storage_accounting,
        )?;
    }
    if let Some(animation) = &image_content.animation {
        validate_animation_content_state(
            &image_content.decoded_image,
            Some(animation),
            image_content.animation_frame_index,
            image_content.animation_loop_count,
            image_content.animation_elapsed_nanos,
            image_content.is_animation_running,
            image_content.is_animation_loading,
        )?;
        for animation_frame in animation.list_frames() {
            let decoded_frame_image = animation_frame.clone_decoded_image();
            validate_image_pixels(&decoded_frame_image)?;
            if image_storage_accounting
                .decoded_image_pointer_addresses
                .insert(Arc::as_ptr(&decoded_frame_image) as usize)
            {
                add_image_storage_accounting_byte_count(
                    animation_frame.get_decoded_image().rgba_bytes.len(),
                    image_storage_accounting,
                )?;
            }
        }
    } else {
        validate_animation_content_state(
            &image_content.decoded_image,
            None,
            image_content.animation_frame_index,
            image_content.animation_loop_count,
            image_content.animation_elapsed_nanos,
            image_content.is_animation_running,
            image_content.is_animation_loading,
        )?;
    }
    if let Some(sixel_source) = &image_content.sixel {
        add_image_storage_accounting_byte_count(
            sixel_source.get_sixel_storage_byte_count()?,
            image_storage_accounting,
        )?;
    }
    Ok(())
}

fn validate_pixel_storage(
    decoded_image: &Arc<crate::graphics::DecodedImage>,
    image_storage_accounting: &mut ImageStorageAccounting,
) -> Result<(), String> {
    validate_image_pixels(decoded_image)?;
    if !image_storage_accounting
        .decoded_image_pointer_addresses
        .insert(Arc::as_ptr(decoded_image) as usize)
    {
        return Ok(());
    }
    add_image_storage_accounting_byte_count(
        decoded_image.rgba_bytes.len(),
        image_storage_accounting,
    )
}

fn validate_image_pixels(decoded_image: &Arc<crate::graphics::DecodedImage>) -> Result<(), String> {
    let image_pixel_width = usize::try_from(decoded_image.pixel_width)
        .map_err(|_| "image width cannot be represented on this platform".to_owned())?;
    let image_pixel_height = usize::try_from(decoded_image.pixel_height)
        .map_err(|_| "image height cannot be represented on this platform".to_owned())?;
    let expected_rgba_byte_count = crate::graphics::compute_rgba_byte_count(
        GraphicsProtocol::Kitty,
        image_pixel_width,
        image_pixel_height,
    )
    .map_err(|error| error.to_string())?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err("image RGBA length does not match its dimensions".to_owned());
    }
    Ok(())
}

fn add_image_storage_accounting_byte_count(
    requested_byte_count: usize,
    image_storage_accounting: &mut ImageStorageAccounting,
) -> Result<(), String> {
    let used_byte_count = image_storage_accounting.used_byte_count;
    image_storage_accounting.used_byte_count = used_byte_count
        .checked_add(requested_byte_count)
        .ok_or_else(|| {
            ImagePlacementError::StorageLimit {
                used_byte_count,
                requested_byte_count,
                byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
            }
            .to_string()
        })?;
    if image_storage_accounting.used_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT {
        return Err(ImagePlacementError::StorageLimit {
            used_byte_count,
            requested_byte_count,
            byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
        }
        .to_string());
    }
    Ok(())
}

const ZERO_FRAME_DELAY_NANOSECOND_COUNT: u64 = 8_000_000;

struct AdvancedImageAnimation {
    image_content: Arc<ImageContent>,
    has_pixel_changes: bool,
}

impl TerminalState {
    /// Return the time until the next retained animation frame is due.
    pub(crate) fn get_next_image_animation_delay(&self) -> Option<std::time::Duration> {
        self.list_image_animation_contents()
            .values()
            .filter_map(|image_content| {
                get_image_animation_delay_nanoseconds(image_content)
                    .map(convert_nanoseconds_to_duration)
            })
            .min()
    }

    /// Advance every retained animation and report whether visible pixels changed.
    pub(crate) fn advance_image_animations(&mut self, elapsed: std::time::Duration) -> bool {
        let elapsed_nanoseconds = elapsed.as_nanos().min(u128::from(u64::MAX));
        let advanced_image_content_by_id = self
            .list_image_animation_contents()
            .into_iter()
            .filter_map(|(image_content_id, image_content)| {
                advance_image_animation_content(&image_content, elapsed_nanoseconds)
                    .map(|advanced_image_animation| (image_content_id, advanced_image_animation))
            })
            .collect::<BTreeMap<_, _>>();
        if advanced_image_content_by_id.is_empty() {
            return false;
        }

        let mut has_pixel_changes = false;
        for placement in self.primary_image_placements.iter_mut().chain(
            self.native_images
                .iter_mut()
                .map(|source| &mut source.placement),
        ) {
            if let Some(advanced_image_animation) =
                advanced_image_content_by_id.get(&placement.image_content.image_content_id)
            {
                has_pixel_changes |= advanced_image_animation.has_pixel_changes;
                apply_image_animation_content(placement, &advanced_image_animation.image_content);
            }
        }
        for placement in &mut self.primary_image_history {
            if let Some(advanced_image_animation) =
                advanced_image_content_by_id.get(&placement.image_content.image_content_id)
            {
                has_pixel_changes |= advanced_image_animation.has_pixel_changes;
                apply_image_animation_history_content(
                    placement,
                    &advanced_image_animation.image_content,
                );
            }
        }
        for placement in &mut self.alternate_image_placements {
            if let Some(advanced_image_animation) =
                advanced_image_content_by_id.get(&placement.image_content.image_content_id)
            {
                has_pixel_changes |= advanced_image_animation.has_pixel_changes;
                apply_image_animation_content(placement, &advanced_image_animation.image_content);
            }
        }
        for kitty_image in &mut self.kitty_images {
            if let Some(advanced_image_animation) =
                advanced_image_content_by_id.get(&kitty_image.image_content.image_content_id)
            {
                has_pixel_changes |= advanced_image_animation.has_pixel_changes;
                kitty_image.image_content = Arc::clone(&advanced_image_animation.image_content);
                let mut updated_image_record = kitty_image.image_record.as_ref().clone();
                updated_image_record.image =
                    Arc::clone(&advanced_image_animation.image_content.decoded_image);
                updated_image_record.animation =
                    advanced_image_animation.image_content.animation.clone();
                kitty_image.image_record = Arc::new(updated_image_record);
            }
        }
        has_pixel_changes
    }

    fn list_image_animation_contents(&self) -> BTreeMap<ImageContentId, Arc<ImageContent>> {
        let mut image_content_by_id = BTreeMap::new();
        for placement in self
            .primary_image_placements
            .iter()
            .chain(self.native_images.iter().map(|source| &source.placement))
        {
            image_content_by_id
                .entry(placement.image_content.image_content_id)
                .or_insert_with(|| Arc::clone(&placement.image_content));
        }
        for placement in &self.primary_image_history {
            image_content_by_id
                .entry(placement.image_content.image_content_id)
                .or_insert_with(|| Arc::clone(&placement.image_content));
        }
        for placement in &self.alternate_image_placements {
            image_content_by_id
                .entry(placement.image_content.image_content_id)
                .or_insert_with(|| Arc::clone(&placement.image_content));
        }
        for kitty_image in &self.kitty_images {
            image_content_by_id
                .entry(kitty_image.image_content.image_content_id)
                .or_insert_with(|| Arc::clone(&kitty_image.image_content));
        }
        image_content_by_id.retain(|_, image_content| image_content.animation.is_some());
        image_content_by_id
    }
}

fn get_image_animation_delay_nanoseconds(image_content: &ImageContent) -> Option<u128> {
    if !image_content.is_animation_running {
        return None;
    }
    let animation = image_content.animation.as_ref()?;
    let animation_frame = animation
        .list_frames()
        .get(image_content.animation_frame_index as usize)?;
    Some(
        compute_animation_frame_delay_nanoseconds(animation_frame)
            .saturating_sub(u128::from(image_content.animation_elapsed_nanos)),
    )
}

fn compute_animation_frame_delay_nanoseconds(
    animation_frame: &koshi_image::AnimationFrame,
) -> u128 {
    if animation_frame.is_gapless() {
        return 0;
    }
    let frame_delay = animation_frame.get_frame_delay();
    let numerator = u128::from(frame_delay.get_numerator_ms());
    if numerator == 0 {
        return u128::from(ZERO_FRAME_DELAY_NANOSECOND_COUNT);
    }
    (numerator
        .saturating_mul(1_000_000)
        .saturating_add(u128::from(frame_delay.get_denominator_ms()).saturating_sub(1)))
        / u128::from(frame_delay.get_denominator_ms())
}

fn convert_nanoseconds_to_duration(nanosecond_count: u128) -> std::time::Duration {
    std::time::Duration::from_nanos(nanosecond_count.min(u128::from(u64::MAX)) as u64)
}

fn compute_animation_cycle_nanoseconds(animation: &DecodedAnimation) -> u128 {
    animation
        .list_frames()
        .iter()
        .map(compute_animation_frame_delay_nanoseconds)
        .sum()
}

fn advance_image_animation_content(
    image_content: &Arc<ImageContent>,
    elapsed_nanoseconds: u128,
) -> Option<AdvancedImageAnimation> {
    let animation = image_content.animation.as_ref()?;
    if !image_content.is_animation_running {
        return None;
    }
    let animation_frame_count = animation.get_frame_count();
    let mut current_animation_frame_index =
        usize::try_from(image_content.animation_frame_index).ok()?;
    if current_animation_frame_index >= animation_frame_count {
        return None;
    }
    let mut current_animation_loop_count = image_content.animation_loop_count;
    let mut remaining_animation_nanoseconds =
        u128::from(image_content.animation_elapsed_nanos).saturating_add(elapsed_nanoseconds);
    let previous_animation_frame_index = current_animation_frame_index;
    let mut is_animation_running = true;
    let mut zero_delay_transition_count = 0usize;
    let animation_cycle_nanoseconds = compute_animation_cycle_nanoseconds(animation);
    loop {
        if !image_content.is_animation_loading
            && current_animation_frame_index == 0
            && animation_cycle_nanoseconds != 0
        {
            let complete_cycle_count =
                remaining_animation_nanoseconds / animation_cycle_nanoseconds;
            if complete_cycle_count != 0 {
                match animation.get_loop_policy() {
                    koshi_image::LoopPolicy::Infinite => {
                        remaining_animation_nanoseconds %= animation_cycle_nanoseconds;
                    }
                    koshi_image::LoopPolicy::Finite(total_playbacks) => {
                        let available_cycle_count = u128::from(
                            total_playbacks.saturating_sub(current_animation_loop_count),
                        );
                        if complete_cycle_count >= available_cycle_count {
                            current_animation_frame_index =
                                get_last_visible_animation_frame_index(animation);
                            current_animation_loop_count = total_playbacks;
                            remaining_animation_nanoseconds = 0;
                            is_animation_running = false;
                            break;
                        }
                        current_animation_loop_count = current_animation_loop_count.saturating_add(
                            u32::try_from(complete_cycle_count).unwrap_or(u32::MAX),
                        );
                        remaining_animation_nanoseconds %= animation_cycle_nanoseconds;
                    }
                }
            }
        }
        let animation_frame_delay_nanoseconds = compute_animation_frame_delay_nanoseconds(
            &animation.list_frames()[current_animation_frame_index],
        );
        if remaining_animation_nanoseconds < animation_frame_delay_nanoseconds {
            break;
        }
        remaining_animation_nanoseconds -= animation_frame_delay_nanoseconds;
        if animation_frame_delay_nanoseconds == 0 {
            zero_delay_transition_count = zero_delay_transition_count.saturating_add(1);
            if zero_delay_transition_count > animation_frame_count {
                current_animation_frame_index = get_last_visible_animation_frame_index(animation);
                current_animation_loop_count = 0;
                remaining_animation_nanoseconds = 0;
                is_animation_running = false;
                break;
            }
        } else {
            zero_delay_transition_count = 0;
        }
        if current_animation_frame_index + 1 < animation_frame_count {
            current_animation_frame_index += 1;
            continue;
        }
        if image_content.is_animation_loading {
            current_animation_frame_index = get_last_visible_animation_frame_index(animation);
            remaining_animation_nanoseconds = 0;
            is_animation_running = false;
            break;
        }
        match animation.get_loop_policy() {
            koshi_image::LoopPolicy::Infinite => {
                current_animation_frame_index = 0;
            }
            koshi_image::LoopPolicy::Finite(total_playbacks) => {
                if current_animation_loop_count.saturating_add(1) >= total_playbacks {
                    current_animation_frame_index =
                        get_last_visible_animation_frame_index(animation);
                    current_animation_loop_count = total_playbacks;
                    remaining_animation_nanoseconds = 0;
                    is_animation_running = false;
                    break;
                }
                current_animation_loop_count = current_animation_loop_count.saturating_add(1);
                current_animation_frame_index = 0;
            }
        }
    }

    if animation_frame_count > 1
        && animation.list_frames()[current_animation_frame_index].is_gapless()
    {
        current_animation_frame_index = get_last_visible_animation_frame_index(animation);
    }
    let new_animation_elapsed_nanoseconds = u64::try_from(remaining_animation_nanoseconds).ok()?;
    if current_animation_frame_index == previous_animation_frame_index
        && current_animation_loop_count == image_content.animation_loop_count
        && new_animation_elapsed_nanoseconds == image_content.animation_elapsed_nanos
        && is_animation_running == image_content.is_animation_running
    {
        return None;
    }

    let mut advanced_image_content = (**image_content).clone();
    advanced_image_content.decoded_image =
        animation.list_frames()[current_animation_frame_index].clone_decoded_image();
    advanced_image_content.animation_frame_index =
        u32::try_from(current_animation_frame_index).ok()?;
    advanced_image_content.animation_loop_count = current_animation_loop_count;
    advanced_image_content.animation_elapsed_nanos = new_animation_elapsed_nanoseconds;
    advanced_image_content.is_animation_running = is_animation_running;
    Some(AdvancedImageAnimation {
        image_content: Arc::new(advanced_image_content),
        has_pixel_changes: current_animation_frame_index != previous_animation_frame_index,
    })
}

fn get_last_visible_animation_frame_index(animation: &DecodedAnimation) -> usize {
    animation
        .list_frames()
        .iter()
        .rposition(|animation_frame| !animation_frame.is_gapless())
        .unwrap_or(0)
}

fn apply_image_animation_content(
    placement: &mut ImagePlacement,
    image_content: &Arc<ImageContent>,
) {
    let mut updated_image_record = placement.image_record.as_ref().clone();
    updated_image_record.image = Arc::clone(&image_content.decoded_image);
    updated_image_record.animation = image_content.animation.clone();
    let raster = match placement.raster.as_ref() {
        Some(_) => match raster::rebuild_raster_image(&updated_image_record, &placement.plan) {
            Ok(raster) => raster,
            Err(_) => return,
        },
        None => None,
    };
    placement.image_record = Arc::new(updated_image_record);
    placement.image_content = Arc::clone(image_content);
    placement.raster = raster;
}

fn apply_image_animation_history_content(
    placement: &mut PrimaryHistoryImagePlacement,
    image_content: &Arc<ImageContent>,
) {
    let mut updated_image_record = placement.image_record.as_ref().clone();
    updated_image_record.image = Arc::clone(&image_content.decoded_image);
    updated_image_record.animation = image_content.animation.clone();
    let raster = match placement.raster.as_ref() {
        Some(_) => match raster::rebuild_raster_image(&updated_image_record, &placement.plan) {
            Ok(raster) => raster,
            Err(_) => return,
        },
        None => None,
    };
    placement.image_record = Arc::new(updated_image_record);
    placement.image_content = Arc::clone(image_content);
    placement.raster = raster;
}

impl TerminalState {
    fn take_image_state_snapshot(&mut self) -> ImageStateSnapshot {
        ImageStateSnapshot {
            native_images: std::mem::take(&mut self.native_images),
            native_fragment_count_by_image_source_id: std::mem::take(
                &mut self.native_fragment_count_by_image_source_id,
            ),
            primary_image_placements: std::mem::take(&mut self.primary_image_placements),
            primary_image_history: std::mem::take(&mut self.primary_image_history),
            alternate_image_placements: std::mem::take(&mut self.alternate_image_placements),
            kitty_images: std::mem::take(&mut self.kitty_images),
            next_image_placement_id: std::mem::replace(
                &mut self.next_image_placement_id,
                default_next_image_placement_id(),
            ),
            next_image_content_id: std::mem::replace(
                &mut self.next_image_content_id,
                default_next_image_content_id(),
            ),
        }
    }

    fn set_image_state_snapshot(&mut self, image_state_snapshot: ImageStateSnapshot) {
        self.native_images = image_state_snapshot.native_images;
        self.native_fragment_count_by_image_source_id =
            image_state_snapshot.native_fragment_count_by_image_source_id;
        self.primary_image_placements = image_state_snapshot.primary_image_placements;
        self.primary_image_history = image_state_snapshot.primary_image_history;
        self.alternate_image_placements = image_state_snapshot.alternate_image_placements;
        self.kitty_images = image_state_snapshot.kitty_images;
        self.next_image_placement_id = image_state_snapshot.next_image_placement_id;
        self.next_image_content_id = image_state_snapshot.next_image_content_id;
    }

    /// Every image content the terminal state retains: Kitty uploads, the
    /// placements of both screens, the primary-screen history, and the native
    /// image sources.
    fn list_retained_image_contents(&self) -> impl Iterator<Item = &Arc<ImageContent>> {
        self.kitty_images
            .iter()
            .map(|kitty_image| &kitty_image.image_content)
            .chain(
                self.primary_image_placements
                    .iter()
                    .map(|placement| &placement.image_content),
            )
            .chain(
                self.primary_image_history
                    .iter()
                    .map(|placement| &placement.image_content),
            )
            .chain(
                self.alternate_image_placements
                    .iter()
                    .map(|placement| &placement.image_content),
            )
            .chain(
                self.native_images
                    .iter()
                    .map(|source| &source.placement.image_content),
            )
    }

    fn allocate_image_content_id(&mut self) -> Result<ImageContentId, ImagePlacementError> {
        let used_image_content_ids = self
            .list_retained_image_contents()
            .map(|image_content| image_content.image_content_id)
            .collect::<HashSet<_>>();
        let mut candidate_image_content_id = self.next_image_content_id;
        loop {
            let next_image_content_id = candidate_image_content_id
                .get_raw_image_content_id()
                .checked_add(1)
                .and_then(ImageContentId::from_raw_image_content_id)
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            if !used_image_content_ids.contains(&candidate_image_content_id) {
                self.next_image_content_id = next_image_content_id;
                return Ok(candidate_image_content_id);
            }
            candidate_image_content_id = next_image_content_id;
        }
    }

    /// Return the retained image whose pixels equal `image`, or `image` itself.
    ///
    /// Two transfers that decode to the same width, height and RGBA bytes share
    /// one `Arc`.
    pub(crate) fn get_or_share_image_pixels(
        &self,
        image: Arc<crate::graphics::DecodedImage>,
    ) -> Arc<crate::graphics::DecodedImage> {
        self.list_retained_image_contents()
            .map(|content| &content.decoded_image)
            .find(|kept| kept.as_ref() == image.as_ref())
            .cloned()
            .unwrap_or(image)
    }

    pub(super) fn allocate_image_content(
        &mut self,
        image_record: Arc<ImageRecord>,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let image_content_id = self.allocate_image_content_id()?;
        Ok(Arc::new(ImageContent {
            image_content_id,
            decoded_image: Arc::clone(&image_record.image),
            animation: image_record.animation.clone(),
            animation_frame_index: 0,
            animation_loop_count: 0,
            animation_elapsed_nanos: 0,
            is_animation_running: image_record.animation.is_some(),
            is_animation_loading: false,
            sixel: None,
        }))
    }

    pub(super) fn allocate_sixel_image_content(
        &mut self,
        image_record: Arc<ImageRecord>,
        sixel_source: SixelImageSource,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let image_content_id = self.allocate_image_content_id()?;
        Ok(Arc::new(ImageContent {
            image_content_id,
            decoded_image: Arc::clone(&image_record.image),
            animation: image_record.animation.clone(),
            animation_frame_index: 0,
            animation_loop_count: 0,
            animation_elapsed_nanos: 0,
            is_animation_running: image_record.animation.is_some(),
            is_animation_loading: false,
            sixel: Some(sixel_source),
        }))
    }

    fn get_image_content_for_record(
        &mut self,
        image_record: &Arc<ImageRecord>,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        if let Some(image_id) = get_kitty_image_id(image_record) {
            if let Some(image_content) = self
                .kitty_images
                .iter()
                .rev()
                .find(|kitty_image| {
                    kitty_image.display.image_id == Some(image_id)
                        && Arc::ptr_eq(
                            &kitty_image.image_content.decoded_image,
                            &image_record.image,
                        )
                })
                .map(|kitty_image| Arc::clone(&kitty_image.image_content))
            {
                return Ok(image_content);
            }
            if let Some(image_content) = self
                .primary_image_placements
                .iter()
                .chain(&self.alternate_image_placements)
                .map(|placement| (&placement.image_record, &placement.image_content))
                .chain(
                    self.primary_image_history
                        .iter()
                        .map(|placement| (&placement.image_record, &placement.image_content)),
                )
                .find(|(existing_image_record, _)| {
                    get_kitty_image_id(existing_image_record) == Some(image_id)
                        && Arc::ptr_eq(&existing_image_record.image, &image_record.image)
                })
                .map(|(_, image_content)| Arc::clone(image_content))
            {
                return Ok(image_content);
            }
        }
        self.allocate_image_content(Arc::clone(image_record))
    }

    /// Return image placements whose anchors are on the active live screen in
    /// insertion order. Primary placements in retained history are returned by
    /// [`list_image_placements_for_view`](Self::list_image_placements_for_view).
    #[must_use]
    pub fn list_image_placements(&self) -> Vec<ImagePlacement> {
        let mut placements = self.list_active_image_placements().to_vec();
        coverage::append_derived_image_placements(
            &mut placements,
            self.list_native_image_placements(self.get_active_grid(), 0)
                .into_iter()
                .filter_map(|placement| placement.into_live_image_placement(0)),
        );
        placements
    }

    /// Return the image portions visible at the requested scrollback offset.
    /// The complete image scale is retained when a view edge clips its cells.
    #[must_use]
    pub fn list_image_placements_for_view(&self, view_row_offset: usize) -> Vec<ImagePlacement> {
        if self.active_screen == Screen::Alternate {
            let (row_count, column_count) = self.alternate.get_grid_dimensions();
            let mut virtual_placements =
                self.list_virtual_image_placements(self.alternate.as_ref(), 0);
            let mut absolute = self
                .alternate_image_placements
                .iter()
                .cloned()
                .map(|placement| AbsoluteImagePlacement {
                    image_placement_id: placement.image_placement_id,
                    image_record: placement.image_record,
                    image_content: placement.image_content,
                    anchor: (u64::from(placement.anchor.0), placement.anchor.1),
                    column_count: placement.column_count,
                    row_count: placement.row_count,
                    plan: placement.plan,
                    raster: placement.raster,
                })
                .collect::<Vec<_>>();
            self.remap_relative_absolute_placements(&mut absolute, &virtual_placements);
            virtual_placements
                .extend(self.list_native_image_placements(self.alternate.as_ref(), 0));
            let mut placements = absolute
                .into_iter()
                .filter_map(|placement| {
                    placement
                        .clip_to_visible_area(0, u64::from(row_count), column_count)?
                        .into_live_image_placement(0)
                })
                .collect::<Vec<_>>();
            coverage::append_derived_image_placements(
                &mut placements,
                virtual_placements
                    .into_iter()
                    .filter_map(|placement| placement.into_live_image_placement(0)),
            );
            return placements;
        }
        let (view_grid, scrolled_row_count) = self.scrolled_view(view_row_offset);
        let (row_count, column_count) = view_grid.get_grid_dimensions();
        let view_top_row = self
            .scrollback
            .get_total_pushed_line_count()
            .saturating_sub(scrolled_row_count as u64);
        let view_end_row = view_top_row.saturating_add(u64::from(row_count));
        let mut placements = self
            .list_primary_absolute_image_placements()
            .into_iter()
            .collect::<Vec<_>>();
        let mut virtual_placements =
            self.list_virtual_image_placements(view_grid.as_ref(), view_top_row);
        self.remap_relative_absolute_placements(&mut placements, &virtual_placements);
        virtual_placements
            .extend(self.list_native_image_placements(view_grid.as_ref(), view_top_row));
        let mut visible = placements
            .into_iter()
            .filter_map(|placement| {
                placement
                    .clip_to_visible_area(view_top_row, view_end_row, column_count)?
                    .into_live_image_placement(view_top_row)
            })
            .collect::<Vec<_>>();
        coverage::append_derived_image_placements(
            &mut visible,
            virtual_placements.into_iter().filter_map(|placement| {
                placement
                    .clip_to_visible_area(view_top_row, view_end_row, column_count)?
                    .into_live_image_placement(view_top_row)
            }),
        );
        visible
    }

    fn list_virtual_image_placements(
        &self,
        terminal_grid: &Grid,
        view_top_row: u64,
    ) -> Vec<AbsoluteImagePlacement> {
        let mut kitty_image_index_by_identity = HashMap::new();
        for (kitty_image_index, kitty_image) in self.kitty_images.iter().enumerate() {
            if kitty_image.is_virtual_placement
                && kitty_image.virtual_screen == Some(self.active_screen)
            {
                if let Some(kitty_image_id) = kitty_image.display.image_id {
                    kitty_image_index_by_identity.insert(
                        (kitty_image_id, kitty_image.display.placement_id),
                        kitty_image_index,
                    );
                    kitty_image_index_by_identity.insert((kitty_image_id, None), kitty_image_index);
                }
            }
        }
        let mut placeholder_cells_by_kitty_image_index: BTreeMap<
            usize,
            Vec<(u16, u16, ResolvedPlaceholder)>,
        > = BTreeMap::new();
        for (row_index, grid_row_cells) in terminal_grid.list_rows().iter().enumerate() {
            let mut previous_placeholder = None;
            for (column_index, terminal_cell) in grid_row_cells.iter().enumerate() {
                let resolved_placeholder =
                    terminal_cell
                        .image_placeholder()
                        .and_then(|raw_placeholder| {
                            resolve_placeholder(raw_placeholder, previous_placeholder)
                        });
                previous_placeholder = resolved_placeholder;
                let Some(resolved_placeholder) = resolved_placeholder else {
                    continue;
                };
                let kitty_image_id = resolved_placeholder.image_id
                    | (u32::from(resolved_placeholder.image_id_most_significant_byte) << 24);
                if let Some(&kitty_image_index) = kitty_image_index_by_identity
                    .get(&(kitty_image_id, resolved_placeholder.placement_id))
                {
                    placeholder_cells_by_kitty_image_index
                        .entry(kitty_image_index)
                        .or_default()
                        .push((row_index as u16, column_index as u16, resolved_placeholder));
                }
            }
        }
        let mut virtual_image_placements = Vec::new();
        for (kitty_image_index, placeholder_cells) in placeholder_cells_by_kitty_image_index {
            let kitty_image = &self.kitty_images[kitty_image_index];
            let Ok(prepared) = raster::prepare_image_with_raster_plan(
                &kitty_image.image_record,
                self.cell_size,
                terminal_grid.get_grid_dimensions(),
            ) else {
                continue;
            };
            let column_count = prepared.plan.geometry.full_size.column_count;
            let row_count = prepared.plan.geometry.full_size.row_count;
            let source_placement = ImagePlacement::from_image_record(
                0,
                Arc::clone(&kitty_image.image_record),
                Arc::clone(&kitty_image.image_content),
                prepared.plan,
                column_count,
                row_count,
                prepared.raster,
            );
            let mut placement_runs = Vec::new();
            for (row, column, resolved_placeholder) in placeholder_cells {
                if resolved_placeholder.source_row_index < row_count
                    && resolved_placeholder.source_column_index < column_count
                {
                    coverage::append_image_placement_cell(
                        &mut placement_runs,
                        &source_placement,
                        row,
                        column,
                        resolved_placeholder.source_row_index,
                        resolved_placeholder.source_column_index,
                    );
                }
            }
            virtual_image_placements.extend(
                coverage::merge_image_placement_rows(placement_runs)
                    .into_iter()
                    .filter_map(|placement| {
                        AbsoluteImagePlacement::from_live_image_placement(placement, view_top_row)
                    }),
            );
        }
        virtual_image_placements
    }

    fn remap_relative_absolute_placements(
        &self,
        placements: &mut [AbsoluteImagePlacement],
        virtual_placements: &[AbsoluteImagePlacement],
    ) {
        let mut relative_image_targets: Vec<RelativeImageTarget> = Vec::new();
        let mut virtual_entry_index_by_identity = HashMap::new();
        for placement in virtual_placements {
            let image_identity = (
                placement.image_record.display.image_id,
                placement.image_record.display.placement_id,
            );
            let anchor = (
                i64::try_from(placement.anchor.0)
                    .unwrap_or(i64::MAX)
                    .saturating_sub(i64::from(placement.plan.geometry.cell_offset.row)),
                i64::from(placement.anchor.1)
                    .saturating_sub(i64::from(placement.plan.geometry.cell_offset.column)),
            );
            if let Some(&virtual_entry_index) = virtual_entry_index_by_identity.get(&image_identity)
            {
                let relative_image_entry: &mut RelativeImageTarget =
                    &mut relative_image_targets[virtual_entry_index];
                if let Some(current_anchor) = relative_image_entry.1.as_mut() {
                    current_anchor.0 = current_anchor.0.min(anchor.0);
                    current_anchor.1 = current_anchor.1.min(anchor.1);
                }
            } else {
                virtual_entry_index_by_identity
                    .insert(image_identity, relative_image_targets.len());
                relative_image_targets.push((Arc::clone(&placement.image_record), Some(anchor)));
            }
        }
        let virtual_count = relative_image_targets.len();
        relative_image_targets.extend(placements.iter().map(|placement| {
            (
                Arc::clone(&placement.image_record),
                Some((
                    i64::try_from(placement.anchor.0).unwrap_or(i64::MAX),
                    i64::from(placement.anchor.1),
                )),
            )
        }));
        for (placement_index, placement) in placements.iter_mut().enumerate() {
            if placement.image_record.display.relative_image_id.is_none() {
                continue;
            }
            let Ok(Some(anchor)) = resolve_relative_image_target(
                placement_index + virtual_count,
                &relative_image_targets,
                &mut HashSet::new(),
                0,
            ) else {
                placement.row_count = 0;
                placement.column_count = 0;
                continue;
            };
            let removed_rows = anchor
                .0
                .saturating_neg()
                .max(0)
                .min(i64::from(placement.row_count)) as u16;
            let removed_columns = anchor
                .1
                .saturating_neg()
                .max(0)
                .min(i64::from(placement.column_count)) as u16;
            placement.row_count -= removed_rows;
            placement.column_count -= removed_columns;
            placement.plan.geometry.cell_offset.row += removed_rows;
            placement.plan.geometry.cell_offset.column += removed_columns;
            if let (Ok(row), Ok(column)) = (
                u64::try_from(anchor.0.max(0)),
                u16::try_from(anchor.1.max(0)),
            ) {
                placement.anchor = (row, column);
            } else {
                placement.row_count = 0;
                placement.column_count = 0;
            }
        }
    }

    pub(super) fn list_primary_absolute_image_placements(&self) -> Vec<AbsoluteImagePlacement> {
        self.list_primary_absolute_image_placements_at(
            self.scrollback.get_total_pushed_line_count(),
        )
    }

    fn list_primary_absolute_image_placements_at(
        &self,
        live_top: u64,
    ) -> Vec<AbsoluteImagePlacement> {
        let mut placements = self
            .primary_image_history
            .iter()
            .cloned()
            .map(PrimaryHistoryImagePlacement::into_absolute_image_placement)
            .collect::<Vec<_>>();
        placements.extend(
            self.primary_image_placements
                .iter()
                .cloned()
                .filter_map(|placement| {
                    AbsoluteImagePlacement::from_live_image_placement(placement, live_top)
                }),
        );
        placements
    }

    pub(super) fn remap_primary_image_placements<PlacementMapper>(
        &mut self,
        old_live_top_row: u64,
        mut placement_mapper: PlacementMapper,
    ) where
        PlacementMapper: FnMut(u64, u16) -> Option<(u64, u16)>,
    {
        let mut placements = self
            .list_primary_absolute_image_placements_at(old_live_top_row)
            .into_iter()
            .filter_map(|mut placement| {
                if placement.image_record.display.relative_image_id.is_some() {
                    return Some(placement);
                }
                let (removed, mapped_anchor) = (0..placement.row_count).find_map(|removed| {
                    let old_row = placement.anchor.0.checked_add(u64::from(removed))?;
                    placement_mapper(old_row, placement.anchor.1).map(|anchor| (removed, anchor))
                })?;
                placement.plan.geometry.cell_offset.row = placement
                    .plan
                    .geometry
                    .cell_offset
                    .row
                    .checked_add(removed)?;
                placement.row_count -= removed;
                Some(placement.with_image_anchor(mapped_anchor))
            })
            .collect::<Vec<_>>();
        self.set_primary_absolute_image_placements(&mut placements);
    }

    pub(super) fn remap_alternate_image_placements<PlacementMapper>(
        &mut self,
        mut placement_mapper: PlacementMapper,
    ) where
        PlacementMapper: FnMut(u16, u16) -> Option<(u16, u16)>,
    {
        let (rows, columns) = self.alternate.get_grid_dimensions();
        self.alternate_image_placements = std::mem::take(&mut self.alternate_image_placements)
            .into_iter()
            .filter_map(|mut placement| {
                if placement.image_record.display.relative_image_id.is_some() {
                    return Some(placement);
                }
                let (removed, anchor) = (0..placement.row_count).find_map(|removed| {
                    placement_mapper(placement.anchor.0.checked_add(removed)?, placement.anchor.1)
                        .map(|anchor| (removed, anchor))
                })?;
                placement.plan.geometry.cell_offset.row = placement
                    .plan
                    .geometry
                    .cell_offset
                    .row
                    .checked_add(removed)?;
                placement.row_count -= removed;
                let placement = placement.with_image_anchor(anchor);
                AbsoluteImagePlacement::from_live_image_placement(placement, 0)?
                    .clip_to_visible_area(0, u64::from(rows), columns)?
                    .into_live_image_placement(0)
            })
            .collect();
    }

    fn set_primary_absolute_image_placements(
        &mut self,
        placements: &mut Vec<AbsoluteImagePlacement>,
    ) {
        placements.sort_unstable_by_key(|placement| placement.image_placement_id);
        self.primary_image_placements.clear();
        self.primary_image_history.clear();

        let history_len = self.scrollback.get_retained_line_count() as u64;
        let live_top = self.scrollback.get_total_pushed_line_count();
        let oldest_history_row = live_top.saturating_sub(history_len);
        let live_end = live_top.saturating_add(u64::from(self.primary.get_grid_dimensions().0));
        let grid_columns = self.primary.get_grid_dimensions().1;

        for mut placement in placements.drain(..) {
            if placement.image_record.display.relative_image_id.is_some() {
                placement.anchor.0 = live_top;
                if let Some(placement) = placement.into_live_image_placement(live_top) {
                    self.primary_image_placements.push(placement);
                }
                continue;
            }
            let Some(placement) =
                placement.clip_to_visible_area(oldest_history_row, live_end, grid_columns)
            else {
                continue;
            };
            if placement.anchor.0 < live_top {
                self.primary_image_history.push(
                    PrimaryHistoryImagePlacement::from_absolute_image_placement(placement),
                );
            } else if let Some(placement) = placement.into_live_image_placement(live_top) {
                self.primary_image_placements.push(placement);
            }
        }
    }

    /// Apply one decoded image record to terminal image state.
    pub(crate) fn apply_image_record(
        &mut self,
        image_record: &ImageRecord,
    ) -> Result<(), ImagePlacementError> {
        self.apply_image_record_with_sixel_options(image_record, None, false, None)
    }

    pub(crate) fn apply_sixel_image_record(
        &mut self,
        image_record: &ImageRecord,
        is_sixel_scrolling: bool,
        should_move_cursor_right: bool,
        sixel_source: SixelImageSource,
    ) -> Result<(), ImagePlacementError> {
        self.apply_image_record_with_sixel_options(
            image_record,
            Some(is_sixel_scrolling),
            should_move_cursor_right,
            Some(sixel_source),
        )
    }

    fn apply_image_record_with_sixel_options(
        &mut self,
        image_record: &ImageRecord,
        is_sixel_scrolling: Option<bool>,
        should_move_cursor_right: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(), ImagePlacementError> {
        if image_record.protocol != GraphicsProtocol::Kitty {
            let used_image_storage_byte_count = self.get_image_storage_byte_count();
            let mut candidate_terminal_state = self.clone();
            let image_record_result = candidate_terminal_state.apply_image_record_inner(
                image_record,
                is_sixel_scrolling,
                should_move_cursor_right,
                sixel_source,
            );
            return match image_record_result {
                Ok((_, cursor_movement)) => {
                    if let Some(cursor_movement) = cursor_movement {
                        candidate_terminal_state.apply_image_cursor_movement(cursor_movement);
                    }
                    let requested_byte_count = candidate_terminal_state
                        .get_image_storage_byte_count()
                        .saturating_sub(used_image_storage_byte_count);
                    candidate_terminal_state.ensure_image_storage(requested_byte_count, None)?;
                    *self = candidate_terminal_state;
                    Ok(())
                }
                Err(image_placement_error) => Err(image_placement_error),
            };
        }

        let original_image_state_snapshot = self.take_image_state_snapshot();
        let staged_image_state_snapshot = original_image_state_snapshot.clone();
        self.set_image_state_snapshot(staged_image_state_snapshot);
        let image_record_result = self.apply_image_record_inner(
            image_record,
            is_sixel_scrolling,
            should_move_cursor_right,
            sixel_source,
        );
        let candidate_image_state = self.take_image_state_snapshot();
        match image_record_result {
            Ok((_, cursor_movement)) => {
                self.set_image_state_snapshot(candidate_image_state);
                if let Some(cursor_movement) = cursor_movement {
                    self.apply_image_cursor_movement(cursor_movement);
                }
                Ok(())
            }
            Err(image_placement_error) => {
                self.set_image_state_snapshot(original_image_state_snapshot);
                Err(image_placement_error)
            }
        }
    }

    fn apply_image_record_inner(
        &mut self,
        image_record: &ImageRecord,
        is_sixel_scrolling: Option<bool>,
        should_move_cursor_right: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(crate::graphics::ImageDisplay, Option<ImageCursorMovement>), ImagePlacementError>
    {
        let mut image_record = image_record.clone();
        self.retain_kitty_image_upload(&mut image_record)?;
        if image_record.display.is_unicode_placeholder
            && image_record.action != ImageAction::Transmit
        {
            self.apply_virtual_kitty_image_placement(&image_record.display)?;
            self.ensure_image_storage(
                image_record.image.rgba_bytes.len(),
                get_kitty_image_id(&image_record),
            )?;
            return Ok((image_record.display, None));
        }
        let is_relative = image_record.display.relative_image_id.is_some()
            || image_record.display.relative_placement_id.is_some();
        let should_move_cursor = image_record.display.should_move_cursor && !is_relative;
        let cursor_movement = match image_record.action {
            ImageAction::Transmit => None,
            ImageAction::Display | ImageAction::TransmitAndDisplay => {
                let (image_column_count, image_row_count) = self.place_image(
                    &mut image_record,
                    is_sixel_scrolling.unwrap_or(false),
                    sixel_source,
                )?;
                Some(if is_sixel_scrolling.is_some() {
                    ImageCursorMovement::SixelCursorMove {
                        cursor_row: self.compute_sixel_cursor_row(
                            image_record.anchor.0,
                            image_row_count,
                            is_sixel_scrolling.unwrap_or(false),
                        ),
                        cursor_column: image_record.anchor.1,
                        column_count: image_column_count,
                        should_move_cursor_right,
                    }
                } else {
                    ImageCursorMovement::RegularCursorMove {
                        column_count: image_column_count,
                        row_count: image_row_count,
                    }
                })
            }
        };
        if image_record.protocol == GraphicsProtocol::Kitty {
            self.ensure_image_storage(
                image_record.image.rgba_bytes.len(),
                get_kitty_image_id(&image_record),
            )?;
        }
        let should_move_cursor =
            should_move_cursor && image_record.protocol != GraphicsProtocol::Iterm2;
        Ok((
            image_record.display,
            cursor_movement.filter(|_| should_move_cursor),
        ))
    }

    fn place_image(
        &mut self,
        image_record: &mut ImageRecord,
        is_sixel_scrolling: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(u16, u16), ImagePlacementError> {
        if image_record.display.is_unicode_placeholder {
            return Err(ImagePlacementError::UnsupportedPlacement);
        }
        if let Some(anchor) = self.resolve_relative_anchor(image_record, None)? {
            image_record.anchor = (
                u16::try_from(anchor.0).unwrap_or(0),
                u16::try_from(anchor.1).unwrap_or(0),
            );
            image_record.display.should_move_cursor = false;
        }
        image_record.compute_source_rect()?;
        let prepared_image = raster::prepare_image_with_raster_plan(
            image_record,
            self.cell_size,
            self.get_active_grid().get_grid_dimensions(),
        )?;
        let image_column_count = prepared_image.column_count;
        let image_row_count = prepared_image.row_count;
        let image_column_count = u16::try_from(image_column_count).map_err(|_| {
            ImagePlacementError::DimensionsTooLarge {
                column_count: image_column_count,
                row_count: image_row_count,
            }
        })?;
        let image_row_count = u16::try_from(image_row_count).map_err(|_| {
            ImagePlacementError::DimensionsTooLarge {
                column_count: u32::from(image_column_count),
                row_count: image_row_count,
            }
        })?;
        if image_column_count == 0 || image_row_count == 0 {
            return Err(ImagePlacementError::ZeroSize {
                column_count: u32::from(image_column_count),
                row_count: u32::from(image_row_count),
            });
        }

        let (grid_rows, grid_columns) = self.get_active_grid().get_grid_dimensions();
        let is_relative = image_record.display.relative_image_id.is_some();
        if !is_relative
            && (image_record.anchor.0 >= grid_rows || image_record.anchor.1 >= grid_columns)
        {
            return Err(ImagePlacementError::OutOfBounds {
                anchor_row: image_record.anchor.0,
                anchor_column: image_record.anchor.1,
                column_count: image_column_count,
                row_count: image_row_count,
                grid_rows,
                grid_columns,
            });
        }

        let replacement_slot = get_kitty_placement_identity(image_record)
            .and_then(|identity| self.find_active_image_placement_slot(identity));
        let image_placement_count = self.primary_image_placements.len()
            + self.primary_image_history.len()
            + self.alternate_image_placements.len()
            + self.native_images.len()
            + usize::from(replacement_slot.is_none());
        if image_placement_count > MAX_IMAGE_PLACEMENT_COUNT {
            return Err(ImagePlacementError::TooManyPlacements {
                placement_count: image_placement_count,
                placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
            });
        }
        let image_placement_id = if let Some(slot) = replacement_slot {
            match slot {
                ActiveImagePlacementSlot::Live(placement_index) => {
                    self.list_active_image_placements()[placement_index].image_placement_id
                }
                ActiveImagePlacementSlot::History(placement_index) => {
                    self.primary_image_history[placement_index].image_placement_id
                }
            }
        } else {
            self.allocate_image_placement_id()?
        };
        let content_record = Arc::new(image_record.clone());
        let content = if let Some(sixel) = sixel_source {
            self.allocate_sixel_image_content(Arc::clone(&content_record), sixel)?
        } else {
            self.get_image_content_for_record(&content_record)?
        };
        let image_record = Arc::new(image_record.clone());
        let mut placement = ImagePlacement::from_image_record(
            image_placement_id,
            Arc::clone(&image_record),
            content,
            prepared_image.plan,
            image_column_count,
            image_row_count,
            prepared_image.raster,
        );
        placement.anchor = image_record.anchor;
        if placement.raster.is_some() {
            let retained = self
                .primary_image_placements
                .iter()
                .chain(&self.alternate_image_placements)
                .find_map(|existing| {
                    (Arc::ptr_eq(&existing.image_content, &placement.image_content)
                        && existing.plan.is_same_raster(&placement.plan))
                    .then(|| existing.raster.clone())
                    .flatten()
                })
                .or_else(|| {
                    self.primary_image_history.iter().find_map(|existing| {
                        (Arc::ptr_eq(&existing.image_content, &placement.image_content)
                            && existing.plan.is_same_raster(&placement.plan))
                        .then(|| existing.raster.clone())
                        .flatten()
                    })
                });
            if let Some(retained) = retained {
                placement.raster = Some(Arc::clone(&retained));
            }
        }
        placement.row_count = if is_relative
            || image_record.protocol == GraphicsProtocol::Iterm2
            || is_sixel_scrolling
        {
            image_row_count
        } else {
            image_row_count.min(grid_rows - image_record.anchor.0)
        };
        placement.column_count = if is_relative {
            image_column_count
        } else {
            image_column_count.min(grid_columns - image_record.anchor.1)
        };
        if image_record.protocol != GraphicsProtocol::Kitty {
            self.install_native_image_placement(placement, is_sixel_scrolling)?;
            return Ok((image_column_count, image_row_count));
        }
        match replacement_slot {
            Some(ActiveImagePlacementSlot::Live(placement_index)) => {
                self.list_active_image_placements_mut()[placement_index] = placement;
            }
            Some(ActiveImagePlacementSlot::History(placement_index)) => {
                self.primary_image_history.remove(placement_index);
                self.insert_active_image_placement_in_order(placement);
            }
            None => self.list_active_image_placements_mut().push(placement),
        }

        Ok((image_column_count, image_row_count))
    }

    fn compute_sixel_cursor_row(
        &self,
        anchor_row_index: u16,
        image_row_count: u16,
        is_scrolling: bool,
    ) -> u16 {
        let (grid_rows, _) = self.get_active_grid().get_grid_dimensions();
        let last_row_index = anchor_row_index.saturating_add(image_row_count.saturating_sub(1));
        if !is_scrolling {
            return last_row_index.min(grid_rows.saturating_sub(1));
        }
        let (scroll_region_top_row_index, scroll_region_bottom_row_index) =
            self.get_scroll_region().map_or(
                (0, grid_rows.saturating_sub(1)),
                |(top_row_index, bottom_row_index)| {
                    (
                        top_row_index.min(grid_rows.saturating_sub(1)),
                        bottom_row_index.min(grid_rows.saturating_sub(1)),
                    )
                },
            );
        if scroll_region_top_row_index <= anchor_row_index
            && anchor_row_index <= scroll_region_bottom_row_index
        {
            last_row_index.min(scroll_region_bottom_row_index)
        } else {
            last_row_index.min(grid_rows.saturating_sub(1))
        }
    }

    pub(super) fn refresh_shared_sixel_images(
        &mut self,
        palette: &SixelPalette,
    ) -> Result<(), crate::graphics::GraphicsError> {
        let mut contents = HashMap::new();
        for placement in self.primary_image_placements.iter_mut().chain(
            self.native_images
                .iter_mut()
                .map(|source| &mut source.placement),
        ) {
            refresh_shared_sixel_placement(placement, palette, &mut contents)?;
        }
        for placement in &mut self.primary_image_history {
            refresh_shared_sixel_history_placement(placement, palette, &mut contents)?;
        }
        for placement in &mut self.alternate_image_placements {
            refresh_shared_sixel_placement(placement, palette, &mut contents)?;
        }
        Ok(())
    }

    fn allocate_image_placement_id(&mut self) -> Result<ImagePlacementId, ImagePlacementError> {
        let mut candidate_image_placement_id = self.next_image_placement_id;
        loop {
            if candidate_image_placement_id == 0 {
                return Err(ImagePlacementError::IdentityExhausted);
            }
            let next_image_placement_id = candidate_image_placement_id
                .checked_add(1)
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            let is_placement_id_used = self
                .primary_image_placements
                .iter()
                .map(|placement| placement.image_placement_id)
                .chain(
                    self.primary_image_history
                        .iter()
                        .map(|placement| placement.image_placement_id),
                )
                .chain(
                    self.alternate_image_placements
                        .iter()
                        .map(|placement| placement.image_placement_id),
                )
                .chain(
                    self.native_images
                        .iter()
                        .map(|source| source.placement.image_placement_id),
                )
                .any(|image_placement_id| image_placement_id == candidate_image_placement_id);
            if !is_placement_id_used {
                self.next_image_placement_id = next_image_placement_id;
                return Ok(candidate_image_placement_id);
            }
            candidate_image_placement_id = next_image_placement_id;
        }
    }

    fn list_active_image_placements_mut(&mut self) -> &mut Vec<ImagePlacement> {
        match self.active_screen {
            Screen::Primary => &mut self.primary_image_placements,
            Screen::Alternate => &mut self.alternate_image_placements,
        }
    }

    fn list_active_image_placements(&self) -> &[ImagePlacement] {
        match self.active_screen {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    fn insert_active_image_placement_in_order(&mut self, placement: ImagePlacement) {
        let placements = self.list_active_image_placements_mut();
        let placement_index = placements
            .iter()
            .position(|existing| existing.image_placement_id > placement.image_placement_id)
            .unwrap_or(placements.len());
        placements.insert(placement_index, placement);
    }

    fn find_active_image_placement_slot(
        &self,
        image_identity: (u32, u32),
    ) -> Option<ActiveImagePlacementSlot> {
        match self.active_screen {
            Screen::Primary => self
                .primary_image_placements
                .iter()
                .position(|placement| {
                    get_kitty_placement_identity(placement.get_image_record())
                        == Some(image_identity)
                })
                .map(ActiveImagePlacementSlot::Live)
                .or_else(|| {
                    self.primary_image_history
                        .iter()
                        .position(|placement| {
                            get_kitty_placement_identity(placement.image_record.as_ref())
                                == Some(image_identity)
                        })
                        .map(ActiveImagePlacementSlot::History)
                }),
            Screen::Alternate => self
                .alternate_image_placements
                .iter()
                .position(|placement| {
                    get_kitty_placement_identity(placement.get_image_record())
                        == Some(image_identity)
                })
                .map(ActiveImagePlacementSlot::Live),
        }
    }

    fn remove_kitty_image(&mut self, kitty_image_id: u32) {
        self.primary_image_placements.retain(|placement| {
            placement.image_record.protocol != GraphicsProtocol::Kitty
                || placement.image_record.display.image_id != Some(kitty_image_id)
        });
        self.primary_image_history.retain(|placement| {
            placement.image_record.protocol != GraphicsProtocol::Kitty
                || placement.image_record.display.image_id != Some(kitty_image_id)
        });
        self.alternate_image_placements.retain(|placement| {
            placement.image_record.protocol != GraphicsProtocol::Kitty
                || placement.image_record.display.image_id != Some(kitty_image_id)
        });
    }

    pub(super) fn clear_active_image_placements(&mut self) {
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        let scrollback_top_line_count = self.scrollback.get_total_pushed_line_count();
        let mut relative_anchor_by_placement_id = HashMap::new();
        match self.active_screen {
            Screen::Primary => {
                for image_placement in &self.primary_image_placements {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
                for image_placement in &self.primary_image_history {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
            }
            Screen::Alternate => {
                for image_placement in &self.alternate_image_placements {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                        || image_placement
                            .image_record
                            .display
                            .relative_placement_id
                            .is_some()
                    {
                        if let Ok(Some((anchor_row, anchor_column))) = self.resolve_relative_anchor(
                            &image_placement.image_record,
                            Some(image_placement.image_placement_id),
                        ) {
                            relative_anchor_by_placement_id.insert(
                                image_placement.image_placement_id,
                                (i128::from(anchor_row), anchor_column),
                            );
                        }
                    }
                }
            }
        }
        let is_image_intersecting_live_grid =
            |image_record: &ImageRecord,
             image_placement_id: ImagePlacementId,
             image_anchor_row: i128,
             image_anchor_column: i64,
             image_row_count: u16,
             image_column_count: u16| {
                let is_relative_image = image_record.display.relative_image_id.is_some()
                    || image_record.display.relative_placement_id.is_some();
                let (resolved_anchor_row, resolved_anchor_column) = if is_relative_image {
                    let Some(anchor_position) = relative_anchor_by_placement_id
                        .get(&image_placement_id)
                        .copied()
                    else {
                        return true;
                    };
                    anchor_position
                } else {
                    (image_anchor_row, image_anchor_column)
                };
                resolved_anchor_row < i128::from(grid_row_count)
                    && resolved_anchor_row + i128::from(image_row_count) > 0
                    && resolved_anchor_column < i64::from(grid_column_count)
                    && resolved_anchor_column + i64::from(image_column_count) > 0
            };
        match self.active_screen {
            Screen::Primary => {
                self.primary_image_placements.retain(|image_placement| {
                    !is_image_intersecting_live_grid(
                        &image_placement.image_record,
                        image_placement.image_placement_id,
                        i128::from(image_placement.anchor.0),
                        i64::from(image_placement.anchor.1),
                        image_placement.row_count,
                        image_placement.column_count,
                    )
                });
                self.primary_image_history.retain(|image_placement| {
                    !is_image_intersecting_live_grid(
                        &image_placement.image_record,
                        image_placement.image_placement_id,
                        i128::from(image_placement.anchor.0)
                            - i128::from(scrollback_top_line_count),
                        i64::from(image_placement.anchor.1),
                        image_placement.row_count,
                        image_placement.column_count,
                    )
                });
            }
            Screen::Alternate => self.alternate_image_placements.retain(|image_placement| {
                !is_image_intersecting_live_grid(
                    &image_placement.image_record,
                    image_placement.image_placement_id,
                    i128::from(image_placement.anchor.0),
                    i64::from(image_placement.anchor.1),
                    image_placement.row_count,
                    image_placement.column_count,
                )
            }),
        }
    }

    pub(super) fn clear_alternate_image_placements(&mut self) {
        let (row_count, column_count) = self.alternate.get_grid_dimensions();
        let previous_active_screen = self.active_screen;
        self.active_screen = Screen::Alternate;
        let mut has_removed_image_fragments = false;
        for row_index in 0..row_count {
            has_removed_image_fragments |=
                self.clear_image_fragments_at_cells(row_index, 0, column_count);
        }
        self.active_screen = previous_active_screen;
        self.native_images
            .retain(|native_image| native_image.screen != Screen::Alternate);
        self.alternate_image_placements.clear();
        self.finish_native_fragment_removal(has_removed_image_fragments);
    }

    pub(super) fn clear_primary_image_history(&mut self) {
        self.primary_image_history.clear();
    }

    pub(super) fn clear_all_image_placements(&mut self) {
        self.native_images.clear();
        self.native_fragment_count_by_image_source_id.clear();
        self.kitty_images.clear();
        self.primary_image_placements.clear();
        self.primary_image_history.clear();
        self.alternate_image_placements.clear();
    }

    pub(super) fn clear_images_at_cells(
        &mut self,
        row_index: u16,
        column_index: u16,
        column_count: u16,
    ) {
        let has_removed_image_fragments = self.clear_image_fragments_at_cells(
            row_index,
            column_index,
            column_index.saturating_add(column_count),
        );
        self.finish_native_fragment_removal(has_removed_image_fragments);
    }

    fn apply_image_cursor_movement(&mut self, movement: ImageCursorMovement) {
        match movement {
            ImageCursorMovement::RegularCursorMove {
                column_count,
                row_count,
            } => {
                self.move_cursor_after_image(column_count, row_count);
            }
            ImageCursorMovement::SixelCursorMove {
                cursor_row,
                cursor_column,
                column_count,
                should_move_cursor_right,
            } => self.move_cursor_after_sixel(
                cursor_row,
                cursor_column,
                column_count,
                should_move_cursor_right,
            ),
        }
    }

    fn move_cursor_after_image(&mut self, column_count: u16, row_count: u16) {
        let (grid_rows, grid_columns) = self.get_active_grid().get_grid_dimensions();
        let cursor_position = self.active_cursor_mut();
        cursor_position.row = cursor_position
            .row
            .saturating_add(row_count)
            .min(grid_rows.saturating_sub(1));
        cursor_position.column = cursor_position
            .column
            .saturating_add(column_count)
            .min(grid_columns.saturating_sub(1));
        cursor_position.pending_wrap = false;
    }

    fn move_cursor_after_sixel(
        &mut self,
        mut cursor_row_index: u16,
        cursor_column_index: u16,
        image_column_count: u16,
        should_move_cursor_right: bool,
    ) {
        let (grid_rows, grid_columns) = self.get_active_grid().get_grid_dimensions();
        if grid_rows == 0 || grid_columns == 0 {
            return;
        }
        let (top_row_index, bottom_row_index) = self
            .get_scroll_region()
            .map_or((0, grid_rows - 1), |(top, bottom)| {
                (top.min(grid_rows - 1), bottom.min(grid_rows - 1))
            });
        cursor_row_index = cursor_row_index.min(grid_rows - 1);
        let mut next_cursor_column_index = cursor_column_index;
        if should_move_cursor_right {
            let right_edge =
                u32::from(cursor_column_index).saturating_add(u32::from(image_column_count));
            if right_edge >= u32::from(grid_columns) {
                next_cursor_column_index = 0;
                cursor_row_index = cursor_row_index.saturating_add(1);
            } else {
                next_cursor_column_index = u16::try_from(right_edge).unwrap_or(grid_columns - 1);
            }
        }
        if top_row_index <= bottom_row_index {
            let background_style = self.active_render().style.get_background_fill_style();
            while cursor_row_index > bottom_row_index {
                self.delete_lines_into_scrollback(
                    top_row_index,
                    bottom_row_index,
                    1,
                    background_style,
                );
                cursor_row_index -= 1;
            }
        }
        let cursor_position = self.active_cursor_mut();
        cursor_position.row = cursor_row_index.min(grid_rows - 1);
        cursor_position.column = next_cursor_column_index.min(grid_columns - 1);
        cursor_position.pending_wrap = false;
    }
}

type RelativeImageTarget = (Arc<ImageRecord>, Option<(i64, i64)>);
type RelativeIdentity = (u32, Option<u32>);

fn get_relative_image_identity(image_record: &ImageRecord) -> Option<RelativeIdentity> {
    if image_record.protocol != GraphicsProtocol::Kitty {
        return None;
    }
    let kitty_image_id = image_record
        .display
        .image_id
        .filter(|image_id| *image_id != 0)?;
    Some((
        kitty_image_id,
        image_record
            .display
            .placement_id
            .filter(|candidate_placement_id| *candidate_placement_id != 0),
    ))
}

fn resolve_relative_image_target(
    relative_image_target_index: usize,
    relative_image_targets: &[RelativeImageTarget],
    resolving_relative_image_identities: &mut HashSet<RelativeIdentity>,
    relative_placement_depth: usize,
) -> Result<Option<(i64, i64)>, ImagePlacementError> {
    let relative_image_identity =
        get_relative_image_identity(&relative_image_targets[relative_image_target_index].0);
    if relative_image_identity
        .is_some_and(|image_identity| !resolving_relative_image_identities.insert(image_identity))
    {
        return Err(ImagePlacementError::RelativeCycle);
    }
    if relative_placement_depth > MAX_RELATIVE_PLACEMENT_DEPTH {
        return Err(ImagePlacementError::RelativeDepth);
    }
    let (image_record, anchor) = &relative_image_targets[relative_image_target_index];
    let display = &image_record.display;
    let resolved_anchor = if let Some(parent_image_id) =
        display.relative_image_id.filter(|image_id| *image_id != 0)
    {
        let parent_placement_id = display
            .relative_placement_id
            .filter(|placement_id| *placement_id != 0);
        let parent_image_target_index = relative_image_targets
            .get(..relative_image_target_index)
            .and_then(|preceding_targets| {
                preceding_targets
                    .iter()
                    .rposition(|(parent_image_record, _)| {
                        parent_image_record.protocol == GraphicsProtocol::Kitty
                            && parent_image_record.display.image_id == Some(parent_image_id)
                            && parent_placement_id.is_none_or(|placement_id| {
                                parent_image_record.display.placement_id == Some(placement_id)
                            })
                    })
            })
            .ok_or(ImagePlacementError::ParentNotFound)?;
        resolve_relative_image_target(
            parent_image_target_index,
            relative_image_targets,
            resolving_relative_image_identities,
            relative_placement_depth + 1,
        )?
        .map(|(parent_row_offset, parent_column_offset)| {
            let resolved_row_offset =
                parent_row_offset.checked_add(i64::from(display.relative_row_offset));
            let resolved_column_offset =
                parent_column_offset.checked_add(i64::from(display.relative_column_offset));
            match (resolved_row_offset, resolved_column_offset) {
                (Some(row_offset), Some(column_offset)) => Ok((row_offset, column_offset)),
                _ => Err(ImagePlacementError::RelativeOffsetOutOfBounds {
                    resolved_row: parent_row_offset,
                    resolved_column: parent_column_offset,
                }),
            }
        })
        .transpose()?
    } else if display.relative_image_id.is_some()
        || display.relative_placement_id.is_some()
        || display.relative_column_offset != 0
        || display.relative_row_offset != 0
    {
        return Err(ImagePlacementError::ParentNotFound);
    } else {
        *anchor
    };
    if let Some(relative_image_identity) = relative_image_identity {
        resolving_relative_image_identities.remove(&relative_image_identity);
    }
    Ok(resolved_anchor)
}

fn refresh_shared_sixel_placement(
    placement: &mut ImagePlacement,
    palette: &SixelPalette,
    image_content_by_id: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<(), crate::graphics::GraphicsError> {
    let Some(content) =
        get_refreshed_shared_sixel_content(&placement.image_content, palette, image_content_by_id)?
    else {
        return Ok(());
    };
    let mut refreshed_image_record = placement.image_record.as_ref().clone();
    refreshed_image_record.image = Arc::clone(&content.decoded_image);
    let refreshed_image_record = Arc::new(refreshed_image_record);
    let raster = raster::rebuild_raster_image(&refreshed_image_record, &placement.plan).map_err(
        |reason| crate::graphics::GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            placement_error: reason,
        },
    )?;
    placement.image_record = refreshed_image_record;
    placement.image_content = content;
    placement.raster = raster;
    Ok(())
}

fn refresh_shared_sixel_history_placement(
    placement: &mut PrimaryHistoryImagePlacement,
    palette: &SixelPalette,
    image_content_by_id: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<(), crate::graphics::GraphicsError> {
    let Some(content) =
        get_refreshed_shared_sixel_content(&placement.image_content, palette, image_content_by_id)?
    else {
        return Ok(());
    };
    let mut refreshed_image_record = placement.image_record.as_ref().clone();
    refreshed_image_record.image = Arc::clone(&content.decoded_image);
    let refreshed_image_record = Arc::new(refreshed_image_record);
    let raster = raster::rebuild_raster_image(&refreshed_image_record, &placement.plan).map_err(
        |reason| crate::graphics::GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            placement_error: reason,
        },
    )?;
    placement.image_record = refreshed_image_record;
    placement.image_content = content;
    placement.raster = raster;
    Ok(())
}

fn get_refreshed_shared_sixel_content(
    image_content: &Arc<ImageContent>,
    palette: &SixelPalette,
    image_content_by_id: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<Option<Arc<ImageContent>>, crate::graphics::GraphicsError> {
    let Some(sixel_image_source) = image_content
        .sixel
        .as_ref()
        .filter(|sixel_image_source| sixel_image_source.is_shared_palette)
    else {
        return Ok(None);
    };
    if let Some(image_content) = image_content_by_id.get(&image_content.image_content_id) {
        return Ok(Some(Arc::clone(image_content)));
    }
    let sixel_image_source = sixel_image_source.with_shared_sixel_palette(palette);
    let decoded_image = sixel_image_source.resolve_sixel_image(palette)?;
    let refreshed_image_content = Arc::new(ImageContent {
        image_content_id: image_content.image_content_id,
        decoded_image,
        animation: image_content.animation.clone(),
        animation_frame_index: image_content.animation_frame_index,
        animation_loop_count: image_content.animation_loop_count,
        animation_elapsed_nanos: image_content.animation_elapsed_nanos,
        is_animation_running: image_content.is_animation_running,
        is_animation_loading: image_content.is_animation_loading,
        sixel: Some(sixel_image_source),
    });
    image_content_by_id.insert(
        image_content.image_content_id,
        Arc::clone(&refreshed_image_content),
    );
    Ok(Some(refreshed_image_content))
}

#[derive(Clone, Copy)]
struct ResolvedPlaceholder {
    image_id: u32,
    placement_id: Option<u32>,
    source_row_index: u16,
    source_column_index: u16,
    image_id_most_significant_byte: u8,
}

fn find_placeholder_anchor(grid: &Grid, kitty_image: &KittyImage) -> Option<(i64, i64)> {
    find_placeholder_anchor_in_rows(
        kitty_image,
        grid.list_rows()
            .iter()
            .enumerate()
            .filter_map(|(row_index, row_cells)| {
                Some((i64::try_from(row_index).ok()?, row_cells.as_slice()))
            }),
    )
}

fn find_placeholder_anchor_in_rows<'a, I>(
    kitty_image: &KittyImage,
    grid_row_entries: I,
) -> Option<(i64, i64)>
where
    I: IntoIterator<Item = (i64, &'a [crate::grid::state::Cell])>,
{
    let requested_image_id = kitty_image
        .display
        .image_id
        .filter(|image_id| *image_id != 0)?;
    let requested_row_count = kitty_image.display.requested_row_count?;
    let requested_column_count = kitty_image.display.requested_column_count?;
    let mut anchor = None;
    for (row_index, row_cells) in grid_row_entries {
        let mut previous_placeholder = None;
        for (column_index, cell) in row_cells.iter().enumerate() {
            let Some(raw_placeholder) = cell.image_placeholder() else {
                previous_placeholder = None;
                continue;
            };
            let Some(placeholder) = resolve_placeholder(raw_placeholder, previous_placeholder)
            else {
                previous_placeholder = None;
                continue;
            };
            previous_placeholder = Some(placeholder);
            let resolved_image_id = placeholder.image_id
                | (u32::from(placeholder.image_id_most_significant_byte) << 24);
            if resolved_image_id != requested_image_id
                || (placeholder.placement_id.is_some()
                    && placeholder.placement_id != kitty_image.display.placement_id)
                || u32::from(placeholder.source_row_index) >= requested_row_count
                || u32::from(placeholder.source_column_index) >= requested_column_count
            {
                continue;
            }
            let placeholder_column_offset = i64::try_from(column_index).ok()?;
            let anchor_row_offset =
                row_index.checked_sub(i64::from(placeholder.source_row_index))?;
            let anchor_column_offset = placeholder_column_offset
                .checked_sub(i64::from(placeholder.source_column_index))?;
            anchor = Some(anchor.map_or(
                (anchor_row_offset, anchor_column_offset),
                |previous_anchor: (i64, i64)| {
                    (
                        previous_anchor.0.min(anchor_row_offset),
                        previous_anchor.1.min(anchor_column_offset),
                    )
                },
            ));
        }
    }
    anchor
}

fn find_primary_placeholder_anchor(
    terminal_state: &TerminalState,
    kitty_image: &KittyImage,
) -> Option<(i64, i64)> {
    let live_scrollback_line_count = terminal_state.scrollback.get_total_pushed_line_count();
    let retained_history_start_line_count = live_scrollback_line_count
        .saturating_sub(terminal_state.scrollback.get_retained_line_count() as u64);
    let retained_history_rows = terminal_state
        .scrollback
        .list_retained_lines()
        .iter()
        .enumerate()
        .filter_map(|(history_row_index, (row_cells, _))| {
            let absolute_scrollback_line_count = retained_history_start_line_count
                .checked_add(u64::try_from(history_row_index).ok()?)?;
            let relative_row_index = i64::try_from(
                i128::from(absolute_scrollback_line_count) - i128::from(live_scrollback_line_count),
            )
            .ok()?;
            Some((relative_row_index, row_cells.as_slice()))
        });
    let live_rows = terminal_state
        .primary
        .list_rows()
        .iter()
        .enumerate()
        .filter_map(|(live_row_index, row_cells)| {
            Some((i64::try_from(live_row_index).ok()?, row_cells.as_slice()))
        });
    find_placeholder_anchor_in_rows(kitty_image, retained_history_rows.chain(live_rows))
}

fn resolve_placeholder(
    raw_placeholder: ImagePlaceholder,
    previous_placeholder: Option<ResolvedPlaceholder>,
) -> Option<ResolvedPlaceholder> {
    let previous_placeholder = previous_placeholder.filter(|previous_placeholder| {
        previous_placeholder.image_id == raw_placeholder.image_id
            && previous_placeholder.placement_id == raw_placeholder.placement_id
    });
    let mut source_row_index = raw_placeholder.source_row.unwrap_or(0);
    let mut source_column_index = raw_placeholder.source_column.unwrap_or(0);
    let mut image_id_most_significant_byte = raw_placeholder.image_id_msb.unwrap_or(0);
    if let Some(previous_placeholder) = previous_placeholder {
        if raw_placeholder.source_row.is_none() {
            source_row_index = previous_placeholder.source_row_index;
            source_column_index = previous_placeholder.source_column_index.checked_add(1)?;
            image_id_most_significant_byte = previous_placeholder.image_id_most_significant_byte;
        } else if raw_placeholder.source_column.is_none()
            && source_row_index == previous_placeholder.source_row_index
        {
            source_column_index = previous_placeholder.source_column_index.checked_add(1)?;
            image_id_most_significant_byte = previous_placeholder.image_id_most_significant_byte;
        } else if raw_placeholder.image_id_msb.is_none()
            && source_row_index == previous_placeholder.source_row_index
            && previous_placeholder.source_column_index.checked_add(1) == Some(source_column_index)
        {
            image_id_most_significant_byte = previous_placeholder.image_id_most_significant_byte;
        }
    }
    Some(ResolvedPlaceholder {
        image_id: raw_placeholder.image_id,
        placement_id: raw_placeholder.placement_id,
        source_row_index,
        source_column_index,
        image_id_most_significant_byte,
    })
}

fn get_kitty_placement_identity(image_record: &ImageRecord) -> Option<(u32, u32)> {
    (image_record.protocol == GraphicsProtocol::Kitty)
        .then_some((
            image_record.display.image_id?,
            image_record.display.placement_id?,
        ))
        .filter(|(image_id, placement_id)| *image_id != 0 && *placement_id != 0)
}

fn get_kitty_image_id(image_record: &ImageRecord) -> Option<u32> {
    (image_record.protocol == GraphicsProtocol::Kitty)
        .then_some(image_record.display.image_id)
        .flatten()
        .filter(|image_id| *image_id != 0)
}

const MAX_RELATIVE_PLACEMENT_DEPTH: usize = 8;

impl TerminalState {
    fn resolve_relative_anchor(
        &self,
        image_record: &ImageRecord,
        current_image_placement_id: Option<ImagePlacementId>,
    ) -> Result<Option<(i64, i64)>, ImagePlacementError> {
        if image_record.display.relative_image_id.is_none()
            && image_record.display.relative_placement_id.is_none()
        {
            if image_record.display.relative_column_offset != 0
                || image_record.display.relative_row_offset != 0
            {
                return Err(ImagePlacementError::ParentNotFound);
            }
            return Ok(None);
        }
        if image_record.display.is_unicode_placeholder {
            return Err(ImagePlacementError::VirtualRelative);
        }
        let mut relative_image_targets: Vec<RelativeImageTarget> = self
            .kitty_images
            .iter()
            .filter(|kitty_image| {
                kitty_image.is_virtual_placement
                    && kitty_image.virtual_screen == Some(self.active_screen)
            })
            .map(|kitty_image| {
                (
                    Arc::clone(&kitty_image.image_record),
                    if self.active_screen == Screen::Primary {
                        find_primary_placeholder_anchor(self, kitty_image)
                    } else {
                        find_placeholder_anchor(self.get_active_grid(), kitty_image)
                    },
                )
            })
            .collect();
        let mut current_image_target_index = None;
        if self.active_screen == Screen::Primary {
            let live_scrollback_line_count = self.scrollback.get_total_pushed_line_count();
            for placement in &self.primary_image_history {
                if current_image_placement_id == Some(placement.image_placement_id) {
                    current_image_target_index = Some(relative_image_targets.len());
                }
                relative_image_targets.push((
                    Arc::clone(&placement.image_record),
                    i64::try_from(
                        i128::from(placement.anchor.0) - i128::from(live_scrollback_line_count),
                    )
                    .ok()
                    .map(|relative_row_index| (relative_row_index, i64::from(placement.anchor.1))),
                ));
            }
        }
        for placement in self.list_active_image_placements() {
            if current_image_placement_id == Some(placement.image_placement_id) {
                current_image_target_index = Some(relative_image_targets.len());
            }
            relative_image_targets.push((
                Arc::clone(&placement.image_record),
                Some((i64::from(placement.anchor.0), i64::from(placement.anchor.1))),
            ));
        }
        let relative_image_target_index = current_image_target_index.unwrap_or_else(|| {
            relative_image_targets.push((Arc::new(image_record.clone()), None));
            relative_image_targets.len() - 1
        });
        let resolved_anchor = resolve_relative_image_target(
            relative_image_target_index,
            &relative_image_targets,
            &mut HashSet::new(),
            0,
        )?;
        resolved_anchor
            .ok_or(ImagePlacementError::ParentNotFound)
            .map(Some)
    }
}

fn compute_image_cell_dimensions(
    image_record: &ImageRecord,
) -> Result<(u32, u32), ImagePlacementError> {
    match image_record.protocol {
        GraphicsProtocol::Kitty => compute_kitty_image_cell_dimensions(image_record),
        GraphicsProtocol::Sixel | GraphicsProtocol::Iterm2 => {
            compute_explicit_image_cell_dimensions(image_record)
        }
    }
}

fn compute_kitty_image_cell_dimensions(
    image_record: &ImageRecord,
) -> Result<(u32, u32), ImagePlacementError> {
    if has_kitty_unsupported_image_dimension(image_record) {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            requested_width: image_record.display.requested_width,
            requested_height: image_record.display.requested_height,
        });
    }

    let (_, _, source_pixel_width, source_pixel_height) = image_record.compute_source_rect()?;

    let requested_column_count = image_record
        .display
        .requested_column_count
        .or_else(|| find_requested_cell_count(image_record.display.requested_width));
    let requested_row_count = image_record
        .display
        .requested_row_count
        .or_else(|| find_requested_cell_count(image_record.display.requested_height));
    match (requested_column_count, requested_row_count) {
        (Some(column_count), Some(row_count)) => Ok((column_count, row_count)),
        (Some(column_count), None) => Ok((
            column_count,
            compute_scaled_image_cell_count(
                column_count,
                source_pixel_width,
                source_pixel_height,
                false,
            )?,
        )),
        (None, Some(row_count)) => Ok((
            compute_scaled_image_cell_count(
                row_count,
                source_pixel_width,
                source_pixel_height,
                true,
            )?,
            row_count,
        )),
        (None, None) => Err(ImagePlacementError::MissingCellDimensions {
            requested_width: image_record.display.requested_width,
            requested_height: image_record.display.requested_height,
        }),
    }
}

fn compute_explicit_image_cell_dimensions(
    image_record: &ImageRecord,
) -> Result<(u32, u32), ImagePlacementError> {
    if image_record
        .display
        .requested_width
        .is_some_and(is_non_cell_image_dimension)
        || image_record
            .display
            .requested_height
            .is_some_and(is_non_cell_image_dimension)
    {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            requested_width: image_record.display.requested_width,
            requested_height: image_record.display.requested_height,
        });
    }

    match (
        find_requested_cell_count(image_record.display.requested_width),
        find_requested_cell_count(image_record.display.requested_height),
    ) {
        (Some(column_count), Some(row_count)) => Ok((column_count, row_count)),
        _ => Err(ImagePlacementError::MissingCellDimensions {
            requested_width: image_record.display.requested_width,
            requested_height: image_record.display.requested_height,
        }),
    }
}

fn find_requested_cell_count(requested_dimension: Option<ImageDimension>) -> Option<u32> {
    match requested_dimension {
        Some(ImageDimension::Cells(cell_count)) => Some(cell_count),
        _ => None,
    }
}

fn has_kitty_unsupported_image_dimension(image_record: &ImageRecord) -> bool {
    [
        image_record.display.requested_width,
        image_record.display.requested_height,
    ]
    .into_iter()
    .flatten()
    .any(|dimension| matches!(dimension, ImageDimension::Percent(_)))
}

fn is_non_cell_image_dimension(dimension: ImageDimension) -> bool {
    matches!(
        dimension,
        ImageDimension::Pixels(_) | ImageDimension::Percent(_)
    )
}

fn compute_scaled_image_cell_count(
    fixed_cell_count: u32,
    source_pixel_width: u32,
    source_pixel_height: u32,
    is_width_derived_from_height: bool,
) -> Result<u32, ImagePlacementError> {
    if fixed_cell_count == 0 || source_pixel_width == 0 || source_pixel_height == 0 {
        return Err(ImagePlacementError::ZeroSize {
            column_count: if is_width_derived_from_height {
                0
            } else {
                fixed_cell_count
            },
            row_count: if is_width_derived_from_height {
                fixed_cell_count
            } else {
                0
            },
        });
    }
    let (scaled_dimension_numerator, scaled_dimension_denominator) = if is_width_derived_from_height
    {
        (
            u64::from(fixed_cell_count) * u64::from(source_pixel_width),
            u64::from(source_pixel_height),
        )
    } else {
        (
            u64::from(fixed_cell_count) * u64::from(source_pixel_height),
            u64::from(source_pixel_width),
        )
    };
    let scaled = scaled_dimension_numerator
        .checked_add(scaled_dimension_denominator - 1)
        .map(|scaled_numerator| scaled_numerator / scaled_dimension_denominator)
        .and_then(|scaled_numerator| u32::try_from(scaled_numerator).ok())
        .ok_or(ImagePlacementError::DimensionsTooLarge {
            column_count: if is_width_derived_from_height {
                0
            } else {
                fixed_cell_count
            },
            row_count: if is_width_derived_from_height {
                fixed_cell_count
            } else {
                0
            },
        })?;
    Ok(scaled.max(1))
}
