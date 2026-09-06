//! Image placements that belong to one terminal screen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroU64;
use std::ops::Deref;
use std::sync::Arc;

use koshi_core::geometry::{ImageCellGeometry, Point, Size};
use koshi_sixel::{IndexedImage, SixelPalette};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub use koshi_image::ImagePlacementError;

use crate::graphics::{
    DecodedAnimation, GraphicsProtocol, ImageAction, ImageDimension, ImageRecord, MAX_IMAGE_SIDE,
};
use crate::grid::state::{Grid, ImagePlaceholder};
use crate::state::{Screen, TerminalState};

mod kitty;
mod raster;
mod scroll;

#[cfg(test)]
mod tests;

/// The terminal-local identity assigned to a placement without a replacement
/// identity from the protocol.
pub type ImagePlacementId = u64;

/// The maximum number of image placements retained by one terminal state.
pub(crate) const MAX_IMAGE_PLACEMENTS: usize = 4_096;

/// The maximum RGBA storage retained by one terminal state.
pub(crate) const MAX_IMAGE_STORAGE_BYTES: usize = crate::graphics::MAX_IMAGE_BYTES;

/// A checked identity for one canonical image source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub(super) struct ImageContentId(NonZeroU64);

impl ImageContentId {
    fn new(value: u64) -> Option<Self> {
        Some(Self(NonZeroU64::new(value)?))
    }

    fn get(self) -> u64 {
        self.0.get()
    }
}

/// Decoded pixels and playback state shared by every placement of one image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImageContent {
    id: ImageContentId,
    image: Arc<crate::graphics::DecodedImage>,
    animation: Option<Arc<DecodedAnimation>>,
    animation_frame: u32,
    animation_loops: u32,
    animation_elapsed_nanos: u64,
    animation_running: bool,
    animation_loading: bool,
    sixel: Option<SixelImageSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SixelImageSource {
    indexed: IndexedImage,
    palette: SixelPalette,
    shared_palette: bool,
}

impl SixelImageSource {
    pub(crate) fn new(indexed: IndexedImage, palette: SixelPalette, shared_palette: bool) -> Self {
        Self {
            indexed,
            palette,
            shared_palette,
        }
    }

    pub(crate) fn resolved(
        &self,
        shared_palette: &SixelPalette,
    ) -> Result<Arc<crate::graphics::DecodedImage>, crate::graphics::GraphicsError> {
        let palette = if self.shared_palette {
            shared_palette
        } else {
            &self.palette
        };
        Ok(Arc::new(self.indexed.resolve(palette, palette.color(0))?))
    }

    fn with_shared_palette(&self, palette: &SixelPalette) -> Self {
        Self {
            indexed: self.indexed.clone(),
            palette: palette.clone(),
            shared_palette: self.shared_palette,
        }
    }

    fn storage_bytes(&self) -> Result<usize, String> {
        let pixels = usize::try_from(self.indexed.width())
            .ok()
            .and_then(|width| {
                usize::try_from(self.indexed.height())
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| "Sixel source dimensions overflow storage accounting".to_owned())?;
        pixels
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|bytes| bytes.checked_add(256 * 3))
            .ok_or_else(|| "Sixel source storage count overflows".to_owned())
    }
}

fn animation_storage_bytes(animation: &DecodedAnimation) -> Result<usize, String> {
    animation.frames().iter().try_fold(0usize, |used, frame| {
        used.checked_add(frame.image().rgba.len())
            .ok_or_else(|| "animation storage count overflows".to_owned())
    })
}

/// A retained Kitty upload and its canonical image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KittyImage {
    record: Arc<ImageRecord>,
    content: Arc<ImageContent>,
    virtual_placement: bool,
    virtual_screen: Option<Screen>,
}

impl serde::Serialize for KittyImage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.record.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for KittyImage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let record = Arc::<ImageRecord>::deserialize(deserializer)?;
        let id = ImageContentId::new(1).expect("one is a valid image content id");
        let content = Arc::new(ImageContent {
            id,
            image: Arc::clone(&record.image),
            animation: record.animation.clone(),
            animation_frame: 0,
            animation_loops: 0,
            animation_elapsed_nanos: 0,
            animation_running: record.animation.is_some(),
            animation_loading: false,
            sixel: None,
        });
        Ok(Self {
            record,
            content,
            virtual_placement: false,
            virtual_screen: None,
        })
    }
}

pub(super) fn legacy_content_for_record(record: &Arc<ImageRecord>) -> Arc<ImageContent> {
    Arc::new(ImageContent {
        id: ImageContentId::new(1).expect("one is a valid image content id"),
        image: Arc::clone(&record.image),
        animation: record.animation.clone(),
        animation_frame: 0,
        animation_loops: 0,
        animation_elapsed_nanos: 0,
        animation_running: record.animation.is_some(),
        animation_loading: false,
        sixel: None,
    })
}

impl Deref for KittyImage {
    type Target = ImageRecord;

    fn deref(&self) -> &Self::Target {
        self.record.as_ref()
    }
}

/// The geometry and pixel transform needed to rebuild one prepared raster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RasterPlan {
    /// The complete cell rectangle and the currently clipped source offset.
    geometry: ImageCellGeometry,
    /// The source rectangle in canonical image pixels.
    source: (u32, u32, u32, u32),
    /// The fitted source size in output pixels.
    target: (u32, u32),
    /// The complete output canvas size in pixels.
    canvas: (u32, u32),
    /// The target's top-left pixel offset in the complete canvas.
    pixel_offset: (u32, u32),
}

impl RasterPlan {
    fn same_raster(&self, other: &Self) -> bool {
        self.source == other.source
            && self.target == other.target
            && self.canvas == other.canvas
            && self.pixel_offset == other.pixel_offset
    }
}

#[derive(Debug, Clone)]
struct ImageStateSnapshot {
    primary_image_placements: Vec<ImagePlacement>,
    primary_image_history: Vec<PrimaryHistoryImagePlacement>,
    alternate_image_placements: Vec<ImagePlacement>,
    kitty_images: Vec<KittyImage>,
    next_image_placement_id: ImagePlacementId,
    next_image_content_id: ImageContentId,
}

enum ImageCursorMovement {
    Regular {
        columns: u16,
        rows: u16,
    },
    Sixel {
        anchor: (u16, u16),
        columns: u16,
        rows: u16,
        cursor_right: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImageRecord {
    protocol: GraphicsProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<Arc<crate::graphics::DecodedImage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    animation: Option<Arc<DecodedAnimation>>,
    action: ImageAction,
    display: crate::graphics::ImageDisplay,
    anchor: (u16, u16),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImagePlacement {
    id: ImagePlacementId,
    record: SerializedImageRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_id: Option<ImageContentId>,
    anchor: (u64, u16),
    columns: u16,
    rows: u16,
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
    record: SerializedImageRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_id: Option<ImageContentId>,
    #[serde(default, skip_serializing_if = "is_false")]
    virtual_placement: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    virtual_screen: Option<Screen>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SerializedImageContent {
    id: ImageContentId,
    image: Arc<crate::graphics::DecodedImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    animation: Option<Arc<DecodedAnimation>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    animation_frame: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    animation_loops: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    animation_elapsed_nanos: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    animation_running: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    animation_loading: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sixel: Option<SixelImageSource>,
}

fn is_zero(value: &(impl PartialEq + Default)) -> bool {
    value == &Default::default()
}

fn is_false(value: &bool) -> bool {
    !*value
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
    used_bytes: usize,
    limit_bytes: usize,
}

fn charge_animation_budget(
    budget: &mut ImageStateBudget,
    animation: Option<&Arc<DecodedAnimation>>,
) -> Result<(), String> {
    if let Some(animation) = animation {
        budget.charge(animation_storage_bytes(animation)?)?;
    }
    Ok(())
}

impl ImageStateBudget {
    pub(super) fn new() -> Self {
        Self::with_limit(MAX_IMAGE_STORAGE_BYTES)
    }

    pub(super) fn with_limit(limit_bytes: usize) -> Self {
        Self {
            used_bytes: 0,
            limit_bytes,
        }
    }

    fn remaining(&self) -> usize {
        self.limit_bytes.saturating_sub(self.used_bytes)
    }

    fn charge(&mut self, requested_bytes: usize) -> Result<(), String> {
        let total_bytes = self
            .used_bytes
            .checked_add(requested_bytes)
            .ok_or_else(|| {
                ImagePlacementError::StorageLimit {
                    used_bytes: self.used_bytes,
                    requested_bytes,
                    limit_bytes: self.limit_bytes,
                }
                .to_string()
            })?;
        if total_bytes > self.limit_bytes {
            return Err(ImagePlacementError::StorageLimit {
                used_bytes: self.used_bytes,
                requested_bytes,
                limit_bytes: self.limit_bytes,
            }
            .to_string());
        }
        self.used_bytes = total_bytes;
        Ok(())
    }
}

struct BudgetedBytesSeed {
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for BudgetedBytesSeed {
    type Value = Vec<u8>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BudgetedBytesVisitor { limit: self.limit })
    }
}

struct BudgetedBytesVisitor {
    limit: usize,
}

impl<'de> Visitor<'de> for BudgetedBytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image byte sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes =
            Vec::with_capacity(sequence.size_hint().unwrap_or_default().min(self.limit));
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == self.limit {
                return Err(de::Error::custom(format!(
                    "image state RGBA data exceeds the remaining storage budget of {} bytes",
                    self.limit
                )));
            }
            bytes.push(byte);
        }
        Ok(bytes)
    }

    fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.limit {
            return Err(E::custom(format!(
                "image state RGBA data exceeds the remaining storage budget of {} bytes",
                self.limit
            )));
        }
        Ok(bytes.to_vec())
    }

    fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.limit {
            return Err(E::custom(format!(
                "image state RGBA data exceeds the remaining storage budget of {} bytes",
                self.limit
            )));
        }
        Ok(bytes)
    }
}

pub(super) struct BudgetedDecodedImageSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for BudgetedDecodedImageSeed<'_> {
    type Value = Arc<crate::graphics::DecodedImage>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut width = None;
        let mut height = None;
        let mut rgba = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "width" => {
                    if width.is_some() {
                        return Err(de::Error::duplicate_field("width"));
                    }
                    width = Some(map.next_value::<u32>()?);
                }
                "height" => {
                    if height.is_some() {
                        return Err(de::Error::duplicate_field("height"));
                    }
                    height = Some(map.next_value::<u32>()?);
                }
                "rgba" => {
                    if rgba.is_some() {
                        return Err(de::Error::duplicate_field("rgba"));
                    }
                    let value = map.next_value_seed(BudgetedBytesSeed {
                        limit: self.budget.remaining(),
                    })?;
                    self.budget
                        .charge(value.capacity())
                        .map_err(de::Error::custom)?;
                    rgba = Some(value);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let width = width.ok_or_else(|| de::Error::missing_field("width"))?;
        let height = height.ok_or_else(|| de::Error::missing_field("height"))?;
        let rgba = rgba.ok_or_else(|| de::Error::missing_field("rgba"))?;
        let image = Arc::new(crate::graphics::DecodedImage {
            width,
            height,
            rgba,
        });
        validate_image_pixels(&image).map_err(de::Error::custom)?;
        Ok(image)
    }
}

struct OptionalImageSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for OptionalImageSeed<'_> {
    type Value = Option<Arc<crate::graphics::DecodedImage>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        BudgetedDecodedImageSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
        .map(Some)
    }
}

pub(super) struct SerializedPlacementsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedPlacementsSeed<'_> {
    type Value = Vec<SerializedImagePlacement>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(SerializedPlacementsVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedPlacementsVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedPlacementsVisitor<'_> {
    type Value = Vec<SerializedImagePlacement>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image placement sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENTS),
        );
        while values.len() < MAX_IMAGE_PLACEMENTS {
            let Some(value) = sequence.next_element_seed(SerializedImagePlacementSeed {
                budget: self.budget,
            })?
            else {
                return Ok(values);
            };
            values.push(value);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                count: MAX_IMAGE_PLACEMENTS + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            }));
        }
        Ok(values)
    }
}

pub(super) struct SerializedKittyImagesSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedKittyImagesSeed<'_> {
    type Value = Vec<SerializedKittyImage>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENTS),
        );
        while values.len() < MAX_IMAGE_PLACEMENTS {
            let Some(value) = sequence.next_element_seed(SerializedKittyImageSeed {
                budget: self.budget,
            })?
            else {
                return Ok(values);
            };
            values.push(value);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                count: MAX_IMAGE_PLACEMENTS + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            }));
        }
        Ok(values)
    }
}

pub(super) struct SerializedContentsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedContentsSeed<'_> {
    type Value = Vec<SerializedImageContent>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(SerializedContentsVisitor {
            budget: self.budget,
        })
    }
}

struct SerializedContentsVisitor<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> Visitor<'de> for SerializedContentsVisitor<'_> {
    type Value = Vec<SerializedImageContent>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded image content sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_IMAGE_PLACEMENTS),
        );
        while values.len() < MAX_IMAGE_PLACEMENTS {
            let Some(value) = sequence.next_element_seed(SerializedImageContentSeed {
                budget: self.budget,
            })?
            else {
                return Ok(values);
            };
            values.push(value);
        }
        if sequence.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(ImagePlacementError::TooManyPlacements {
                count: MAX_IMAGE_PLACEMENTS + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            }));
        }
        Ok(values)
    }
}

struct SerializedImageRecordSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImageRecordSeed<'_> {
    type Value = SerializedImageRecord;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut protocol = None;
        let mut image = None;
        let mut animation = None;
        let mut action = None;
        let mut display = None;
        let mut anchor = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "protocol" => {
                    if protocol.is_some() {
                        return Err(de::Error::duplicate_field("protocol"));
                    }
                    protocol = Some(map.next_value::<GraphicsProtocol>()?);
                }
                "image" => {
                    if image.is_some() {
                        return Err(de::Error::duplicate_field("image"));
                    }
                    image = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "action" => {
                    if action.is_some() {
                        return Err(de::Error::duplicate_field("action"));
                    }
                    action = Some(map.next_value::<ImageAction>()?);
                }
                "display" => {
                    if display.is_some() {
                        return Err(de::Error::duplicate_field("display"));
                    }
                    display = Some(map.next_value::<crate::graphics::ImageDisplay>()?);
                }
                "anchor" => {
                    if anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    anchor = Some(map.next_value::<(u16, u16)>()?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        charge_animation_budget(self.budget, animation.as_ref().and_then(Option::as_ref))
            .map_err(de::Error::custom)?;
        Ok(SerializedImageRecord {
            protocol: protocol.ok_or_else(|| de::Error::missing_field("protocol"))?,
            image: image.unwrap_or_default(),
            animation: animation.unwrap_or_default(),
            action: action.ok_or_else(|| de::Error::missing_field("action"))?,
            display: display.ok_or_else(|| de::Error::missing_field("display"))?,
            anchor: anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
        })
    }
}

struct SerializedImagePlacementSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImagePlacementSeed<'_> {
    type Value = SerializedImagePlacement;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut id = None;
        let mut record = None;
        let mut content_id = None;
        let mut anchor = None;
        let mut columns = None;
        let mut rows = None;
        let mut plan = None;
        let mut geometry = None;
        let mut raster = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "id" => {
                    if id.is_some() {
                        return Err(de::Error::duplicate_field("id"));
                    }
                    id = Some(map.next_value::<ImagePlacementId>()?);
                }
                "record" => {
                    if record.is_some() {
                        return Err(de::Error::duplicate_field("record"));
                    }
                    record = Some(map.next_value_seed(SerializedImageRecordSeed {
                        budget: self.budget,
                    })?);
                }
                "content_id" => {
                    if content_id.is_some() {
                        return Err(de::Error::duplicate_field("content_id"));
                    }
                    content_id = Some(map.next_value::<Option<ImageContentId>>()?);
                }
                "anchor" => {
                    if anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    anchor = Some(map.next_value::<(u64, u16)>()?);
                }
                "columns" => {
                    if columns.is_some() {
                        return Err(de::Error::duplicate_field("columns"));
                    }
                    columns = Some(map.next_value::<u16>()?);
                }
                "rows" => {
                    if rows.is_some() {
                        return Err(de::Error::duplicate_field("rows"));
                    }
                    rows = Some(map.next_value::<u16>()?);
                }
                "plan" => {
                    if plan.is_some() {
                        return Err(de::Error::duplicate_field("plan"));
                    }
                    plan = Some(map.next_value::<Option<RasterPlan>>()?);
                }
                "geometry" => {
                    if geometry.is_some() {
                        return Err(de::Error::duplicate_field("geometry"));
                    }
                    geometry = Some(map.next_value::<Option<ImageCellGeometry>>()?);
                }
                "raster" => {
                    if raster.is_some() {
                        return Err(de::Error::duplicate_field("raster"));
                    }
                    raster = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(SerializedImagePlacement {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            record: record.ok_or_else(|| de::Error::missing_field("record"))?,
            content_id: content_id.unwrap_or_default(),
            anchor: anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
            columns: columns.ok_or_else(|| de::Error::missing_field("columns"))?,
            rows: rows.ok_or_else(|| de::Error::missing_field("rows"))?,
            plan: plan.unwrap_or_default(),
            geometry: geometry.unwrap_or_default(),
            raster: raster.unwrap_or_default(),
        })
    }
}

struct SerializedKittyImageSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedKittyImageSeed<'_> {
    type Value = SerializedKittyImage;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut protocol = None;
        let mut image = None;
        let mut animation = None;
        let mut action = None;
        let mut display = None;
        let mut anchor = None;
        let mut content_id = None;
        let mut virtual_placement = None;
        let mut virtual_screen = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "protocol" => {
                    if protocol.is_some() {
                        return Err(de::Error::duplicate_field("protocol"));
                    }
                    protocol = Some(map.next_value::<GraphicsProtocol>()?);
                }
                "image" => {
                    if image.is_some() {
                        return Err(de::Error::duplicate_field("image"));
                    }
                    image = Some(map.next_value_seed(OptionalImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "action" => {
                    if action.is_some() {
                        return Err(de::Error::duplicate_field("action"));
                    }
                    action = Some(map.next_value::<ImageAction>()?);
                }
                "display" => {
                    if display.is_some() {
                        return Err(de::Error::duplicate_field("display"));
                    }
                    display = Some(map.next_value::<crate::graphics::ImageDisplay>()?);
                }
                "anchor" => {
                    if anchor.is_some() {
                        return Err(de::Error::duplicate_field("anchor"));
                    }
                    anchor = Some(map.next_value::<(u16, u16)>()?);
                }
                "content_id" => {
                    if content_id.is_some() {
                        return Err(de::Error::duplicate_field("content_id"));
                    }
                    content_id = Some(map.next_value::<Option<ImageContentId>>()?);
                }
                "virtual_placement" => {
                    if virtual_placement.is_some() {
                        return Err(de::Error::duplicate_field("virtual_placement"));
                    }
                    virtual_placement = Some(map.next_value::<bool>()?);
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
        charge_animation_budget(self.budget, animation.as_ref().and_then(Option::as_ref))
            .map_err(de::Error::custom)?;
        Ok(SerializedKittyImage {
            record: SerializedImageRecord {
                protocol: protocol.ok_or_else(|| de::Error::missing_field("protocol"))?,
                image: image.unwrap_or_default(),
                animation: animation.unwrap_or_default(),
                action: action.ok_or_else(|| de::Error::missing_field("action"))?,
                display: display.ok_or_else(|| de::Error::missing_field("display"))?,
                anchor: anchor.ok_or_else(|| de::Error::missing_field("anchor"))?,
            },
            content_id: content_id.unwrap_or_default(),
            virtual_placement: virtual_placement.unwrap_or(false),
            virtual_screen: virtual_screen.unwrap_or_default(),
        })
    }
}

struct SerializedImageContentSeed<'a> {
    budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for SerializedImageContentSeed<'_> {
    type Value = SerializedImageContent;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut id = None;
        let mut image = None;
        let mut animation = None;
        let mut animation_frame = None;
        let mut animation_loops = None;
        let mut animation_elapsed_nanos = None;
        let mut animation_running = None;
        let mut animation_loading = None;
        let mut sixel = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "id" => {
                    if id.is_some() {
                        return Err(de::Error::duplicate_field("id"));
                    }
                    id = Some(map.next_value::<ImageContentId>()?);
                }
                "image" => {
                    if image.is_some() {
                        return Err(de::Error::duplicate_field("image"));
                    }
                    image = Some(map.next_value_seed(BudgetedDecodedImageSeed {
                        budget: self.budget,
                    })?);
                }
                "animation" => {
                    if animation.is_some() {
                        return Err(de::Error::duplicate_field("animation"));
                    }
                    animation = Some(map.next_value::<Option<Arc<DecodedAnimation>>>()?);
                }
                "animation_frame" => {
                    if animation_frame.is_some() {
                        return Err(de::Error::duplicate_field("animation_frame"));
                    }
                    animation_frame = Some(map.next_value::<u32>()?);
                }
                "animation_loops" => {
                    if animation_loops.is_some() {
                        return Err(de::Error::duplicate_field("animation_loops"));
                    }
                    animation_loops = Some(map.next_value::<u32>()?);
                }
                "animation_elapsed_nanos" => {
                    if animation_elapsed_nanos.is_some() {
                        return Err(de::Error::duplicate_field("animation_elapsed_nanos"));
                    }
                    animation_elapsed_nanos = Some(map.next_value::<u64>()?);
                }
                "animation_running" => {
                    if animation_running.is_some() {
                        return Err(de::Error::duplicate_field("animation_running"));
                    }
                    animation_running = Some(map.next_value::<bool>()?);
                }
                "animation_loading" => {
                    if animation_loading.is_some() {
                        return Err(de::Error::duplicate_field("animation_loading"));
                    }
                    animation_loading = Some(map.next_value::<bool>()?);
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
        let animation = animation.unwrap_or_default();
        charge_animation_budget(self.budget, animation.as_ref()).map_err(de::Error::custom)?;
        if let Some(source) = &sixel {
            let bytes = source.storage_bytes().map_err(de::Error::custom)?;
            self.budget.charge(bytes).map_err(de::Error::custom)?;
        }
        Ok(SerializedImageContent {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            image: image.ok_or_else(|| de::Error::missing_field("image"))?,
            animation,
            animation_frame: animation_frame.unwrap_or_default(),
            animation_loops: animation_loops.unwrap_or_default(),
            animation_elapsed_nanos: animation_elapsed_nanos.unwrap_or_default(),
            animation_running: animation_running.unwrap_or(false),
            animation_loading: animation_loading.unwrap_or(false),
            sixel,
        })
    }
}

pub(super) struct OptionalContentsSeed<'a> {
    pub(super) budget: &'a mut ImageStateBudget,
}

impl<'de> DeserializeSeed<'de> for OptionalContentsSeed<'_> {
    type Value = Option<Vec<SerializedImageContent>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
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

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        SerializedContentsSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
        .map(Some)
    }
}

impl SerializedImageRecord {
    fn from_record(record: &ImageRecord, include_image: bool) -> Self {
        SerializedImageRecord {
            protocol: record.protocol,
            image: include_image.then(|| Arc::clone(&record.image)),
            animation: include_image.then(|| record.animation.clone()).flatten(),
            action: record.action,
            display: record.display.clone(),
            anchor: record.anchor,
        }
    }

    fn into_record(self, content: Arc<ImageContent>) -> Result<Arc<ImageRecord>, String> {
        if let Some(image) = self.image {
            if image.as_ref() != content.image.as_ref() {
                return Err("image record pixels do not match its content id".to_owned());
            }
        }
        if let Some(animation) = &self.animation {
            if content.animation.as_deref() != Some(animation.as_ref()) {
                return Err("image record animation does not match its content id".to_owned());
            }
        }
        Ok(Arc::new(ImageRecord {
            protocol: self.protocol,
            image: Arc::clone(&content.image),
            animation: content.animation.clone(),
            action: self.action,
            display: self.display,
            anchor: self.anchor,
        }))
    }
}

pub(super) fn serialized_image_placement(
    placement: &ImagePlacement,
) -> Result<SerializedImagePlacement, String> {
    if !Arc::ptr_eq(&placement.record.image, &placement.content.image) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    Ok(SerializedImagePlacement {
        id: placement.id,
        record: SerializedImageRecord::from_record(placement.record(), false),
        content_id: Some(placement.content.id),
        anchor: (u64::from(placement.anchor.0), placement.anchor.1),
        columns: placement.columns,
        rows: placement.rows,
        plan: Some(placement.plan.clone()),
        geometry: None,
        raster: None,
    })
}

pub(super) fn serialized_history_placement(
    placement: &PrimaryHistoryImagePlacement,
) -> Result<SerializedImagePlacement, String> {
    if !Arc::ptr_eq(&placement.record.image, &placement.content.image) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    Ok(SerializedImagePlacement {
        id: placement.id,
        record: SerializedImageRecord::from_record(placement.record.as_ref(), false),
        content_id: Some(placement.content.id),
        anchor: placement.anchor,
        columns: placement.columns,
        rows: placement.rows,
        plan: Some(placement.plan.clone()),
        geometry: None,
        raster: None,
    })
}

pub(super) fn serialized_kitty_image(image: &KittyImage) -> Result<SerializedKittyImage, String> {
    if !Arc::ptr_eq(&image.record.image, &image.content.image) {
        return Err("Kitty image record is not aliased to its content".to_owned());
    }
    Ok(SerializedKittyImage {
        record: SerializedImageRecord::from_record(image.record.as_ref(), false),
        content_id: Some(image.content.id),
        virtual_placement: image.virtual_placement,
        virtual_screen: image.virtual_screen,
    })
}

pub(super) fn serialized_content_table(
    state: &TerminalState,
) -> Result<Vec<SerializedImageContent>, String> {
    let mut contents = BTreeMap::<ImageContentId, Arc<ImageContent>>::new();
    let mut add = |content: &Arc<ImageContent>| -> Result<(), String> {
        if let Some(existing) = contents.get(&content.id) {
            if !Arc::ptr_eq(existing, content) || !Arc::ptr_eq(&existing.image, &content.image) {
                return Err("one image content identity has different pixels".to_owned());
            }
        } else {
            if contents.len() == MAX_IMAGE_PLACEMENTS {
                return Err(ImagePlacementError::TooManyPlacements {
                    count: MAX_IMAGE_PLACEMENTS + 1,
                    limit: MAX_IMAGE_PLACEMENTS,
                }
                .to_string());
            }
            contents.insert(content.id, Arc::clone(content));
        }
        Ok(())
    };
    for placement in &state.primary_image_placements {
        add(&placement.content)?;
    }
    for placement in &state.primary_image_history {
        add(&placement.content)?;
    }
    for placement in &state.alternate_image_placements {
        add(&placement.content)?;
    }
    for image in &state.kitty_images {
        add(&image.content)?;
    }
    Ok(contents
        .into_iter()
        .map(|(id, content)| SerializedImageContent {
            id,
            image: Arc::clone(&content.image),
            animation: content.animation.clone(),
            animation_frame: content.animation_frame,
            animation_loops: content.animation_loops,
            animation_elapsed_nanos: content.animation_elapsed_nanos,
            animation_running: content.animation_running,
            animation_loading: content.animation_loading,
            sixel: content.sixel.clone(),
        })
        .collect())
}

struct RestoreImageBuilder {
    table: BTreeMap<ImageContentId, Arc<ImageContent>>,
    legacy_kitty: BTreeMap<u32, Arc<ImageContent>>,
    referenced: HashSet<ImageContentId>,
    raster_cache: HashMap<RasterCacheKey, Arc<crate::graphics::DecodedImage>>,
    raster_pointers: HashSet<usize>,
    storage_bytes: usize,
    next: ImageContentId,
    new_format: bool,
}

impl RestoreImageBuilder {
    fn new(
        contents: Option<Vec<SerializedImageContent>>,
        next: Option<ImageContentId>,
    ) -> Result<Self, String> {
        let new_format = contents.is_some();
        let mut table = BTreeMap::new();
        let mut bytes = 0usize;
        for content in contents.unwrap_or_default() {
            if table.contains_key(&content.id) {
                return Err("image content identities must be unique".to_owned());
            }
            if table.len() == MAX_IMAGE_PLACEMENTS {
                return Err(ImagePlacementError::TooManyPlacements {
                    count: MAX_IMAGE_PLACEMENTS + 1,
                    limit: MAX_IMAGE_PLACEMENTS,
                }
                .to_string());
            }
            validate_animation_content_state(
                &content.image,
                content.animation.as_ref(),
                content.animation_frame,
                content.animation_loops,
                content.animation_elapsed_nanos,
                content.animation_running,
                content.animation_loading,
            )?;
            table.insert(
                content.id,
                Arc::new(ImageContent {
                    id: content.id,
                    image: content.image,
                    animation: content.animation,
                    animation_frame: content.animation_frame,
                    animation_loops: content.animation_loops,
                    animation_elapsed_nanos: content.animation_elapsed_nanos,
                    animation_running: content.animation_running,
                    animation_loading: content.animation_loading,
                    sixel: content.sixel,
                }),
            );
            bytes = bytes
                .checked_add(table[&content.id].image.rgba.len())
                .ok_or_else(|| "image content storage count overflows".to_owned())?;
            if let Some(animation) = &table[&content.id].animation {
                bytes = bytes
                    .checked_add(animation_storage_bytes(animation)?)
                    .ok_or_else(|| "image content storage count overflows".to_owned())?;
            }
            if let Some(source) = &table[&content.id].sixel {
                bytes = bytes
                    .checked_add(source.storage_bytes()?)
                    .ok_or_else(|| "image content storage count overflows".to_owned())?;
            }
            if bytes > MAX_IMAGE_STORAGE_BYTES {
                return Err("image content storage exceeds the image limit".to_owned());
            }
        }
        let next = next.unwrap_or_else(default_next_image_content_id);
        if table.contains_key(&next) {
            return Err("next image content identity collides with retained content".to_owned());
        }
        if next.get() == u64::MAX {
            return Err("image content identity space is exhausted".to_owned());
        }
        Ok(Self {
            table,
            legacy_kitty: BTreeMap::new(),
            referenced: HashSet::new(),
            raster_cache: HashMap::new(),
            raster_pointers: HashSet::new(),
            storage_bytes: bytes,
            next,
            new_format,
        })
    }

    fn content(
        &mut self,
        record: &SerializedImageRecord,
        content_id: Option<ImageContentId>,
    ) -> Result<Arc<ImageContent>, String> {
        if self.new_format != content_id.is_some() {
            return Err("image records must use the content table consistently".to_owned());
        }
        if content_id.is_some() && record.image.is_some() {
            return Err("content-table image records cannot carry inline pixels".to_owned());
        }
        if let Some(content_id) = content_id {
            let content = self
                .table
                .get(&content_id)
                .cloned()
                .ok_or_else(|| "image placement refers to missing content".to_owned())?;
            self.referenced.insert(content_id);
            return Ok(content);
        }
        let image = record
            .image
            .as_ref()
            .ok_or_else(|| "legacy image record is missing pixels".to_owned())?;
        if let Some(image_id) = kitty_image_id_fields(record) {
            if let Some(content) = self.legacy_kitty.get(&image_id) {
                if content.image.as_ref() != image.as_ref() {
                    return Err("one Kitty image id refers to different pixels".to_owned());
                }
                return Ok(Arc::clone(content));
            }
        }
        let id = self.allocate()?;
        self.add_storage_bytes(image.rgba.len())?;
        let content = Arc::new(ImageContent {
            id,
            image: Arc::clone(image),
            animation: record.animation.clone(),
            animation_frame: 0,
            animation_loops: 0,
            animation_elapsed_nanos: 0,
            animation_running: record.animation.is_some(),
            animation_loading: false,
            sixel: None,
        });
        if let Some(animation) = &record.animation {
            self.add_storage_bytes(animation_storage_bytes(animation)?)?;
        }
        if let Some(image_id) = kitty_image_id_fields(record) {
            self.legacy_kitty.insert(image_id, Arc::clone(&content));
        }
        Ok(content)
    }

    fn allocate(&mut self) -> Result<ImageContentId, String> {
        let mut candidate = self.next;
        loop {
            let next = candidate
                .get()
                .checked_add(1)
                .and_then(ImageContentId::new)
                .ok_or_else(|| "image content identity space is exhausted".to_owned())?;
            if !self.table.contains_key(&candidate)
                && !self
                    .legacy_kitty
                    .values()
                    .any(|content| content.id == candidate)
            {
                self.next = next;
                return Ok(candidate);
            }
            candidate = next;
        }
    }

    fn add_storage_bytes(&mut self, requested_bytes: usize) -> Result<(), String> {
        self.ensure_storage_bytes(requested_bytes)?;
        self.storage_bytes += requested_bytes;
        Ok(())
    }

    fn ensure_storage_bytes(&self, requested_bytes: usize) -> Result<(), String> {
        let used_bytes = self.storage_bytes;
        let total_bytes = used_bytes.checked_add(requested_bytes).ok_or_else(|| {
            ImagePlacementError::StorageLimit {
                used_bytes,
                requested_bytes,
                limit_bytes: MAX_IMAGE_STORAGE_BYTES,
            }
            .to_string()
        })?;
        if total_bytes > MAX_IMAGE_STORAGE_BYTES {
            return Err(ImagePlacementError::StorageLimit {
                used_bytes,
                requested_bytes,
                limit_bytes: MAX_IMAGE_STORAGE_BYTES,
            }
            .to_string());
        }
        Ok(())
    }

    fn cache_raster(
        &mut self,
        key: RasterCacheKey,
        raster: Arc<crate::graphics::DecodedImage>,
    ) -> Result<Arc<crate::graphics::DecodedImage>, String> {
        if let Some(existing) = self.raster_cache.get(&key) {
            return Ok(Arc::clone(existing));
        }
        if self.raster_cache.len() == MAX_IMAGE_PLACEMENTS {
            return Err(ImagePlacementError::TooManyPlacements {
                count: MAX_IMAGE_PLACEMENTS + 1,
                limit: MAX_IMAGE_PLACEMENTS,
            }
            .to_string());
        }
        if self.raster_pointers.insert(Arc::as_ptr(&raster) as usize) {
            self.add_storage_bytes(raster.rgba.len())?;
        }
        self.raster_cache.insert(key, Arc::clone(&raster));
        Ok(raster)
    }

    fn retain_raster(
        &mut self,
        raster: Arc<crate::graphics::DecodedImage>,
    ) -> Result<Arc<crate::graphics::DecodedImage>, String> {
        if self.raster_pointers.insert(Arc::as_ptr(&raster) as usize) {
            self.add_storage_bytes(raster.rgba.len())?;
        }
        Ok(raster)
    }

    fn finish(self) -> Result<ImageContentId, String> {
        if self.new_format && self.referenced.len() != self.table.len() {
            return Err("image content table contains unreferenced entries".to_owned());
        }
        Ok(self.next)
    }
}

fn validate_animation_content_state(
    image: &Arc<crate::graphics::DecodedImage>,
    animation: Option<&Arc<DecodedAnimation>>,
    frame: u32,
    loops: u32,
    elapsed_nanos: u64,
    running: bool,
    loading: bool,
) -> Result<(), String> {
    let Some(animation) = animation else {
        if frame != 0 || loops != 0 || elapsed_nanos != 0 || running || loading {
            return Err("image content has playback state without animation frames".to_owned());
        }
        return Ok(());
    };
    let frame_index = usize::try_from(frame)
        .map_err(|_| ImagePlacementError::AnimationFrameNotFound { frame }.to_string())?;
    let frame_image = animation
        .frames()
        .get(frame_index)
        .ok_or_else(|| ImagePlacementError::AnimationFrameNotFound { frame }.to_string())?
        .image();
    if frame_image != image.as_ref() {
        return Err("image content pixels do not match its current animation frame".to_owned());
    }
    if animation
        .loop_policy()
        .total_playbacks()
        .is_some_and(|total| loops > total)
    {
        return Err("image content playback count exceeds its animation loop policy".to_owned());
    }
    if loading && !running {
        return Err("image content cannot load an animation while stopped".to_owned());
    }
    Ok(())
}

fn kitty_image_id_fields(record: &SerializedImageRecord) -> Option<u32> {
    (record.protocol == GraphicsProtocol::Kitty)
        .then_some(record.display.image_id)
        .flatten()
        .filter(|id| *id != 0)
}

fn restore_plan(
    record: &ImageRecord,
    raw: &SerializedImagePlacement,
    content: &Arc<ImageContent>,
    builder: &mut RestoreImageBuilder,
) -> Result<(RasterPlan, Option<Arc<crate::graphics::DecodedImage>>), String> {
    if let Some(plan) = &raw.plan {
        validate_plan(record, plan, raw.columns, raw.rows)?;
        let raster = match &raw.raster {
            Some(raster) => {
                if (raster.width, raster.height) != plan.canvas {
                    return Err("image raster dimensions do not match its raster plan".to_owned());
                }
                Some(builder.retain_raster(raster.clone())?)
            }
            None => {
                let key = (
                    content.id,
                    plan.source,
                    plan.target,
                    plan.canvas,
                    plan.pixel_offset,
                );
                if let Some(raster) = builder.raster_cache.get(&key) {
                    Some(Arc::clone(raster))
                } else {
                    let is_identity = plan.target == plan.canvas
                        && plan.pixel_offset == (0, 0)
                        && plan.source
                            == record.source_rect().map_err(|error| error.to_string())?
                        && plan.target == (record.image.width, record.image.height);
                    if !is_identity {
                        let raster_bytes = usize::try_from(plan.canvas.0)
                            .ok()
                            .and_then(|width| {
                                usize::try_from(plan.canvas.1).ok().and_then(|height| {
                                    width
                                        .checked_mul(height)
                                        .and_then(|pixels| pixels.checked_mul(4))
                                })
                            })
                            .ok_or_else(|| "image raster plan byte count overflows".to_owned())?;
                        builder.ensure_storage_bytes(raster_bytes)?;
                    }
                    let raster =
                        raster::rebuild(record, plan).map_err(|error| error.to_string())?;
                    raster
                        .map(|raster| builder.cache_raster(key, raster))
                        .transpose()?
                }
            }
        };
        return Ok((plan.clone(), raster));
    }
    let geometry = raw.geometry.unwrap_or(ImageCellGeometry {
        full_size: Size {
            cols: raw.columns,
            rows: raw.rows,
        },
        offset: Point { x: 0, y: 0 },
    });
    validate_geometry(record, raw.columns, raw.rows, Some(geometry))?;
    let raster = raw
        .raster
        .clone()
        .map(|raster| builder.retain_raster(raster))
        .transpose()?;
    Ok((legacy_plan(record, geometry, raster.as_ref()), raster))
}

fn validate_new_format_placement(
    raw: &SerializedImagePlacement,
    builder: &RestoreImageBuilder,
) -> Result<(), String> {
    if !builder.new_format {
        return Ok(());
    }
    if raw.plan.is_none() {
        return Err("content-table image placements require a raster plan".to_owned());
    }
    if raw.geometry.is_some() || raw.raster.is_some() {
        return Err("content-table image placements cannot carry legacy raster fields".to_owned());
    }
    Ok(())
}

fn validate_plan(
    record: &ImageRecord,
    plan: &RasterPlan,
    columns: u16,
    rows: u16,
) -> Result<(), String> {
    validate_image_pixels(&record.image)?;
    if !plan.geometry.contains(Size {
        cols: columns,
        rows,
    }) {
        return Err("image clipping exceeds its complete cell dimensions".to_owned());
    }
    let source = record.source_rect().map_err(|error| error.to_string())?;
    if source != plan.source {
        return Err("image raster plan source does not match its image record".to_owned());
    }
    if plan.target.0 == 0
        || plan.target.1 == 0
        || plan.canvas.0 == 0
        || plan.canvas.1 == 0
        || u64::from(plan.pixel_offset.0) + u64::from(plan.target.0) > u64::from(plan.canvas.0)
        || u64::from(plan.pixel_offset.1) + u64::from(plan.target.1) > u64::from(plan.canvas.1)
    {
        return Err("image raster plan dimensions are invalid".to_owned());
    }
    let source_end_x = plan.source.0.checked_add(plan.source.2);
    let source_end_y = plan.source.1.checked_add(plan.source.3);
    if source_end_x.is_none_or(|end| end > record.image.width)
        || source_end_y.is_none_or(|end| end > record.image.height)
        || plan.source.2 == 0
        || plan.source.3 == 0
    {
        return Err("image raster plan source overflows".to_owned());
    }
    let bytes = usize::try_from(plan.canvas.0)
        .ok()
        .and_then(|width| {
            usize::try_from(plan.canvas.1).ok().and_then(|height| {
                width
                    .checked_mul(height)
                    .and_then(|pixels| pixels.checked_mul(4))
            })
        })
        .ok_or_else(|| "image raster plan byte count overflows".to_owned())?;
    if plan.canvas.0 > MAX_IMAGE_SIDE as u32
        || plan.canvas.1 > MAX_IMAGE_SIDE as u32
        || bytes > MAX_IMAGE_STORAGE_BYTES
    {
        return Err("image raster plan exceeds image limits".to_owned());
    }
    Ok(())
}

fn restore_live_placement(
    raw: SerializedImagePlacement,
    builder: &mut RestoreImageBuilder,
) -> Result<ImagePlacement, String> {
    validate_new_format_placement(&raw, builder)?;
    let id = raw.id;
    if id == 0 {
        return Err("image placement identity must be nonzero".to_owned());
    }
    if raw.columns == 0 || raw.rows == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    if raw.anchor.0 > u64::from(u16::MAX) {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    let content = builder.content(&raw.record, raw.content_id)?;
    let record = raw.record.clone().into_record(Arc::clone(&content))?;
    if !matches!(
        record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    let (plan, raster) = restore_plan(&record, &raw, &content, builder)?;
    let end_row = raw
        .anchor
        .0
        .checked_add(u64::from(raw.rows))
        .ok_or_else(|| "image placement row extent overflows u64".to_owned())?;
    let end_column = u64::from(raw.anchor.1) + u64::from(raw.columns);
    if end_row > u64::from(u16::MAX) + 1 || end_column > u64::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    Ok(ImagePlacement {
        id,
        record,
        content,
        anchor: (raw.anchor.0 as u16, raw.anchor.1),
        columns: raw.columns,
        rows: raw.rows,
        plan,
        raster,
    })
}

fn restore_history_placement(
    raw: SerializedImagePlacement,
    builder: &mut RestoreImageBuilder,
) -> Result<PrimaryHistoryImagePlacement, String> {
    validate_new_format_placement(&raw, builder)?;
    if raw.id == 0 {
        return Err("image placement identity must be nonzero".to_owned());
    }
    if raw.columns == 0 || raw.rows == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    let content = builder.content(&raw.record, raw.content_id)?;
    let record = raw.record.clone().into_record(Arc::clone(&content))?;
    if !matches!(
        record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    let (plan, raster) = restore_plan(&record, &raw, &content, builder)?;
    raw.anchor
        .0
        .checked_add(u64::from(raw.rows))
        .ok_or_else(|| "image placement row extent overflows u64".to_owned())?;
    let end_column = u64::from(raw.anchor.1) + u64::from(raw.columns);
    if end_column > u64::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_owned());
    }
    Ok(PrimaryHistoryImagePlacement {
        id: raw.id,
        record,
        content,
        anchor: raw.anchor,
        columns: raw.columns,
        rows: raw.rows,
        plan,
        raster,
    })
}

pub(super) fn restore_serialized_image_state(
    primary: Vec<SerializedImagePlacement>,
    history: Vec<SerializedImagePlacement>,
    alternate: Vec<SerializedImagePlacement>,
    kitty: Vec<SerializedKittyImage>,
    contents: Option<Vec<SerializedImageContent>>,
    next: Option<ImageContentId>,
) -> Result<RestoredImageState, String> {
    let total_count = primary
        .len()
        .checked_add(history.len())
        .and_then(|count| count.checked_add(alternate.len()))
        .ok_or_else(|| "image placement count overflows".to_owned())?;
    if total_count > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: total_count,
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    if kitty.len() > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: kitty.len(),
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    let mut builder = RestoreImageBuilder::new(contents, next)?;
    let mut primary_ids = HashSet::new();
    if primary
        .iter()
        .any(|placement| !primary_ids.insert(placement.id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let mut history_ids = HashSet::new();
    if history
        .iter()
        .any(|placement| !history_ids.insert(placement.id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let mut alternate_ids = HashSet::new();
    if alternate
        .iter()
        .any(|placement| !alternate_ids.insert(placement.id))
    {
        return Err("image placement identities must be unique per screen".to_owned());
    }
    let primary_image_placements = primary
        .into_iter()
        .map(|raw| restore_live_placement(raw, &mut builder))
        .collect::<Result<Vec<_>, _>>()?;
    let primary_image_history = history
        .into_iter()
        .map(|raw| restore_history_placement(raw, &mut builder))
        .collect::<Result<Vec<_>, _>>()?;
    let alternate_image_placements = alternate
        .into_iter()
        .map(|raw| restore_live_placement(raw, &mut builder))
        .collect::<Result<Vec<_>, _>>()?;
    let mut kitty_images = Vec::with_capacity(kitty.len());
    let mut kitty_ids = HashSet::new();
    for raw in kitty {
        if raw.record.action == ImageAction::Display && !raw.virtual_placement {
            return Err("a retained Kitty upload requires a transmit action".to_owned());
        }
        if raw.virtual_placement
            && (!raw.record.display.unicode_placeholder || raw.virtual_screen.is_none())
        {
            return Err("a virtual Kitty placement has invalid metadata".to_owned());
        }
        let id = kitty_image_id_fields(&raw.record)
            .ok_or_else(|| "a retained Kitty upload requires a nonzero image id".to_owned())?;
        let identity = if raw.virtual_placement {
            (
                id,
                raw.record.display.placement_id,
                true,
                raw.virtual_screen == Some(Screen::Alternate),
            )
        } else {
            (id, None, false, false)
        };
        if !kitty_ids.insert(identity) {
            return Err("retained Kitty image ids must be unique".to_owned());
        }
        let content = builder.content(&raw.record, raw.content_id)?;
        let record = raw.record.clone().into_record(Arc::clone(&content))?;
        kitty_images.push(KittyImage {
            record,
            content,
            virtual_placement: raw.virtual_placement,
            virtual_screen: raw.virtual_screen,
        });
    }
    let mut retained_kitty_ids = kitty_images
        .iter()
        .filter_map(|image| kitty_image_id(image.record.as_ref()))
        .collect::<HashSet<_>>();
    for placement in primary_image_placements
        .iter()
        .chain(&alternate_image_placements)
    {
        let Some(id) = kitty_image_id(placement.record.as_ref()) else {
            continue;
        };
        if retained_kitty_ids.insert(id) {
            let mut upload = placement.record.as_ref().clone();
            upload.action = ImageAction::Transmit;
            upload.display.image_id = Some(id);
            kitty_images.push(KittyImage {
                record: Arc::new(upload),
                content: Arc::clone(&placement.content),
                virtual_placement: false,
                virtual_screen: None,
            });
        }
    }
    for placement in &primary_image_history {
        let Some(id) = kitty_image_id(placement.record.as_ref()) else {
            continue;
        };
        if retained_kitty_ids.insert(id) {
            let mut upload = placement.record.as_ref().clone();
            upload.action = ImageAction::Transmit;
            upload.display.image_id = Some(id);
            kitty_images.push(KittyImage {
                record: Arc::new(upload),
                content: Arc::clone(&placement.content),
                virtual_placement: false,
                virtual_screen: None,
            });
        }
    }
    let next_image_content_id = builder.finish()?;
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
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    #[serde(skip)]
    content: Arc<ImageContent>,
    /// The zero-based row and column of the upper-left covered cell.
    anchor: (u16, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
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
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    #[serde(skip)]
    content: Arc<ImageContent>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
    /// The transform and complete cell geometry used to rebuild the raster.
    plan: RasterPlan,
    /// Pixel-sized output padded to the complete cell rectangle.
    raster: Option<Arc<crate::graphics::DecodedImage>>,
}

/// One placement addressed by the absolute primary row space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AbsoluteImagePlacement {
    /// The terminal-local identity for this placement.
    id: ImagePlacementId,
    /// The complete image record retained with the placement and terminal state.
    record: Arc<ImageRecord>,
    /// The canonical image source shared by this placement's copies.
    content: Arc<ImageContent>,
    /// The absolute primary row and column of the upper-left covered cell.
    anchor: (u64, u16),
    /// The number of covered columns.
    columns: u16,
    /// The number of covered rows.
    rows: u16,
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
    fn new(
        id: ImagePlacementId,
        record: Arc<ImageRecord>,
        content: Arc<ImageContent>,
        plan: RasterPlan,
        columns: u16,
        rows: u16,
        raster: Option<Arc<crate::graphics::DecodedImage>>,
    ) -> Self {
        ImagePlacement {
            id,
            record,
            content,
            anchor: (0, 0),
            columns,
            rows,
            plan,
            raster,
        }
    }

    fn with_anchor(&self, anchor: (u16, u16)) -> Self {
        ImagePlacement {
            id: self.id,
            record: Arc::clone(&self.record),
            content: Arc::clone(&self.content),
            anchor,
            columns: self.columns,
            rows: self.rows,
            plan: self.plan.clone(),
            raster: self.raster.clone(),
        }
    }

    /// Return the terminal-local placement identity.
    #[must_use]
    pub fn id(&self) -> ImagePlacementId {
        self.id
    }

    /// Return the complete image record retained by this placement.
    #[must_use]
    pub fn record(&self) -> &ImageRecord {
        self.record.as_ref()
    }

    /// Return the shared complete image record retained by this placement.
    #[must_use]
    pub fn record_arc(&self) -> Arc<ImageRecord> {
        Arc::clone(&self.record)
    }

    /// Return the canonical source identity used by this placement.
    #[must_use]
    pub fn content_id(&self) -> u64 {
        self.content.id.get()
    }

    /// Return the image pixels prepared for the complete cell rectangle.
    #[must_use]
    pub fn render_record_arc(&self) -> Arc<ImageRecord> {
        let Some(raster) = &self.raster else {
            return self.record_arc();
        };
        let mut record = self.record.as_ref().clone();
        record.image = Arc::clone(raster);
        record.display.width = None;
        record.display.height = None;
        record.display.source_offset_x = None;
        record.display.source_offset_y = None;
        record.display.cell_offset_x = None;
        record.display.cell_offset_y = None;
        Arc::new(record)
    }

    /// Return the zero-based row and column of the placement anchor.
    #[must_use]
    pub fn anchor(&self) -> (u16, u16) {
        self.anchor
    }

    /// Return the complete image size and the clipped top and left cells.
    #[must_use]
    pub fn geometry(&self) -> ImageCellGeometry {
        self.plan.geometry
    }

    /// Return the placement dimensions as `(rows, columns)`.
    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.rows, self.columns)
    }

    /// Return whether (`row`, `column`) is one of the cells covered by this
    /// placement.
    #[must_use]
    pub fn covers(&self, row: u16, column: u16) -> bool {
        u32::from(row) >= u32::from(self.anchor.0)
            && u32::from(column) >= u32::from(self.anchor.1)
            && u32::from(row) < u32::from(self.anchor.0) + u32::from(self.rows)
            && u32::from(column) < u32::from(self.anchor.1) + u32::from(self.columns)
    }

    /// Visit covered cells in row-major order.
    pub fn covered_cells(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        let (anchor_row, anchor_column) = self.anchor;
        (0..self.rows).flat_map(move |row| {
            (0..self.columns).map(move |column| {
                (
                    anchor_row
                        .checked_add(row)
                        .expect("validated image placement row fits in u16"),
                    anchor_column
                        .checked_add(column)
                        .expect("validated image placement column fits in u16"),
                )
            })
        })
    }
}

impl PrimaryHistoryImagePlacement {
    fn from_absolute(placement: AbsoluteImagePlacement) -> Self {
        PrimaryHistoryImagePlacement {
            id: placement.id,
            record: placement.record,
            content: placement.content,
            anchor: placement.anchor,
            columns: placement.columns,
            rows: placement.rows,
            plan: placement.plan,
            raster: placement.raster,
        }
    }

    fn into_absolute(self) -> AbsoluteImagePlacement {
        AbsoluteImagePlacement {
            id: self.id,
            record: self.record,
            content: self.content,
            anchor: self.anchor,
            columns: self.columns,
            rows: self.rows,
            plan: self.plan,
            raster: self.raster.clone(),
        }
    }
}

impl AbsoluteImagePlacement {
    fn from_live(placement: ImagePlacement, live_top: u64) -> Option<Self> {
        Some(AbsoluteImagePlacement {
            id: placement.id,
            record: placement.record,
            content: placement.content,
            anchor: (
                live_top.checked_add(u64::from(placement.anchor.0))?,
                placement.anchor.1,
            ),
            columns: placement.columns,
            rows: placement.rows,
            plan: placement.plan,
            raster: placement.raster,
        })
    }

    fn into_live(self, live_top: u64) -> Option<ImagePlacement> {
        Some(ImagePlacement {
            id: self.id,
            record: self.record,
            content: self.content,
            anchor: (
                u16::try_from(self.anchor.0.checked_sub(live_top)?).ok()?,
                self.anchor.1,
            ),
            columns: self.columns,
            rows: self.rows,
            plan: self.plan,
            raster: self.raster.clone(),
        })
    }

    fn clipped(mut self, top: u64, end: u64, columns: u16) -> Option<Self> {
        let start = self.anchor.0.max(top);
        let bottom = self.anchor.0.checked_add(u64::from(self.rows))?.min(end);
        let right = (u32::from(self.anchor.1) + u32::from(self.columns)).min(u32::from(columns));
        if start >= bottom || right <= u32::from(self.anchor.1) {
            return None;
        }
        let removed = u16::try_from(start - self.anchor.0).ok()?;
        self.plan.geometry.offset.y = self.plan.geometry.offset.y.checked_add(removed)?;
        self.anchor.0 = start;
        self.rows = u16::try_from(bottom - start).ok()?;
        self.columns = u16::try_from(right - u32::from(self.anchor.1)).ok()?;
        Some(self)
    }

    fn with_anchor(mut self, anchor: (u64, u16)) -> Self {
        self.anchor = anchor;
        self
    }
}

impl<'de> serde::Deserialize<'de> for ImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ImagePlacementFields {
            id: ImagePlacementId,
            record: Arc<ImageRecord>,
            anchor: (u16, u16),
            columns: u16,
            rows: u16,
            #[serde(default)]
            geometry: Option<ImageCellGeometry>,
            #[serde(default)]
            plan: Option<RasterPlan>,
            #[serde(default)]
            raster: Option<Arc<crate::graphics::DecodedImage>>,
        }

        let fields = <ImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        if fields.id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }
        if !matches!(
            fields.record.action,
            ImageAction::Display | ImageAction::TransmitAndDisplay
        ) {
            return Err(serde::de::Error::custom(
                "image placement image record must be a display record",
            ));
        }
        if fields.columns == 0 || fields.rows == 0 {
            return Err(serde::de::Error::custom(
                "image placement dimensions must be nonzero",
            ));
        }
        validate_geometry(
            &fields.record,
            fields.columns,
            fields.rows,
            fields
                .plan
                .as_ref()
                .map(|plan| plan.geometry)
                .or(fields.geometry),
        )
        .map_err(serde::de::Error::custom)?;
        let row_end = u32::from(fields.anchor.0) + u32::from(fields.rows);
        let column_end = u32::from(fields.anchor.1) + u32::from(fields.columns);
        if row_end > u32::from(u16::MAX) + 1 || column_end > u32::from(u16::MAX) + 1 {
            return Err(serde::de::Error::custom(
                "image placement coordinate extent does not fit in u16",
            ));
        }

        let record = fields.record;
        let content = legacy_content_for_record(&record);
        let plan = fields.plan.unwrap_or_else(|| {
            legacy_plan(
                &record,
                fields.geometry.unwrap_or(ImageCellGeometry {
                    full_size: Size {
                        cols: fields.columns,
                        rows: fields.rows,
                    },
                    offset: Point { x: 0, y: 0 },
                }),
                fields.raster.as_ref(),
            )
        });
        Ok(ImagePlacement {
            id: fields.id,
            record,
            anchor: fields.anchor,
            columns: fields.columns,
            rows: fields.rows,
            plan,
            content,
            raster: fields.raster,
        })
    }
}

impl<'de> serde::Deserialize<'de> for PrimaryHistoryImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct PrimaryHistoryImagePlacementFields {
            id: ImagePlacementId,
            record: Arc<ImageRecord>,
            anchor: (u64, u16),
            columns: u16,
            rows: u16,
            #[serde(default)]
            geometry: Option<ImageCellGeometry>,
            #[serde(default)]
            plan: Option<RasterPlan>,
            #[serde(default)]
            raster: Option<Arc<crate::graphics::DecodedImage>>,
        }

        let fields =
            <PrimaryHistoryImagePlacementFields as serde::Deserialize>::deserialize(deserializer)?;
        validate_history_placement(
            &fields.record,
            fields.anchor,
            fields.columns,
            fields.rows,
            fields
                .plan
                .as_ref()
                .map(|plan| plan.geometry)
                .or(fields.geometry),
        )
        .map_err(serde::de::Error::custom)?;
        if fields.id == 0 {
            return Err(serde::de::Error::custom(
                "image placement identity must be nonzero",
            ));
        }

        let record = fields.record;
        let content = legacy_content_for_record(&record);
        let plan = fields.plan.unwrap_or_else(|| {
            legacy_plan(
                &record,
                fields.geometry.unwrap_or(ImageCellGeometry {
                    full_size: Size {
                        cols: fields.columns,
                        rows: fields.rows,
                    },
                    offset: Point { x: 0, y: 0 },
                }),
                fields.raster.as_ref(),
            )
        });
        Ok(PrimaryHistoryImagePlacement {
            id: fields.id,
            record,
            anchor: fields.anchor,
            columns: fields.columns,
            rows: fields.rows,
            plan,
            content,
            raster: fields.raster,
        })
    }
}

fn validate_history_placement(
    record: &ImageRecord,
    anchor: (u64, u16),
    columns: u16,
    rows: u16,
    geometry: Option<ImageCellGeometry>,
) -> Result<(), String> {
    if !matches!(
        record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_string());
    }
    if columns == 0 || rows == 0 {
        return Err("image placement dimensions must be nonzero".to_string());
    }
    validate_geometry(record, columns, rows, geometry)?;
    anchor
        .0
        .checked_add(u64::from(rows))
        .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
    let column_end = u32::from(anchor.1) + u32::from(columns);
    if column_end > u32::from(u16::MAX) + 1 {
        return Err("image placement coordinate extent does not fit in u16".to_string());
    }
    Ok(())
}

fn validate_geometry(
    record: &ImageRecord,
    columns: u16,
    rows: u16,
    geometry: Option<ImageCellGeometry>,
) -> Result<(), String> {
    record.source_rect().map_err(|error| error.to_string())?;
    if let Some(geometry) = geometry {
        if !geometry.contains(Size {
            cols: columns,
            rows,
        }) {
            return Err("image clipping exceeds its complete cell dimensions".to_string());
        }
    } else {
        let (record_columns, record_rows) =
            cell_dimensions(record).map_err(|error| error.to_string())?;
        if record_columns != u32::from(columns) || record_rows != u32::from(rows) {
            return Err("image placement dimensions do not match its image record".to_string());
        }
    }
    Ok(())
}

fn legacy_plan(
    record: &ImageRecord,
    geometry: ImageCellGeometry,
    raster: Option<&Arc<crate::graphics::DecodedImage>>,
) -> RasterPlan {
    let source = record
        .source_rect()
        .unwrap_or((0, 0, record.image.width, record.image.height));
    let canvas = raster
        .map(|image| (image.width, image.height))
        .unwrap_or((record.image.width, record.image.height));
    RasterPlan {
        geometry,
        source,
        target: canvas,
        canvas,
        pixel_offset: (0, 0),
    }
}

pub(super) fn default_next_image_placement_id() -> ImagePlacementId {
    1
}

pub(super) fn default_next_image_content_id() -> ImageContentId {
    ImageContentId::new(1).expect("one is a valid image content id")
}

pub(super) fn validate_image_state(fields: &super::TerminalStateFields) -> Result<(), String> {
    if fields.next_image_placement_id == 0 {
        return Err("next image placement identity must be nonzero".to_string());
    }
    let total_count = fields
        .primary_image_placements
        .len()
        .checked_add(fields.primary_image_history.len())
        .and_then(|count| count.checked_add(fields.alternate_image_placements.len()))
        .ok_or_else(|| "image placement count overflows".to_owned())?;
    if total_count > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: total_count,
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    if fields.kitty_images.len() > MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: fields.kitty_images.len(),
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    let mut ids = HashSet::with_capacity(total_count);
    let mut content_ids = BTreeMap::<ImageContentId, (usize, usize)>::new();
    let mut storage_bytes = (0usize, HashSet::new());
    let mut retained_kitty_ids = HashSet::new();
    for image in &fields.kitty_images {
        if image.record.action == ImageAction::Display && !image.virtual_placement {
            return Err("a retained Kitty upload requires a transmit action".to_owned());
        }
        if image.virtual_placement
            && (!image.record.display.unicode_placeholder || image.virtual_screen.is_none())
        {
            return Err("a virtual Kitty placement has invalid metadata".to_owned());
        }
        let image_id = kitty_image_id(image.record.as_ref())
            .ok_or_else(|| "a retained Kitty upload requires a nonzero image id".to_owned())?;
        if !retained_kitty_ids.insert(image_id) {
            return Err("retained Kitty image ids must be unique".to_owned());
        }
        if !Arc::ptr_eq(&image.record.image, &image.content.image) {
            return Err("Kitty image record is not aliased to its content".to_owned());
        }
        validate_content_storage(&image.content, &mut content_ids, &mut storage_bytes)?;
    }
    let mut primary_kitty_identities = HashSet::new();
    for placement in &fields.primary_image_placements {
        validate_live_image_placement(
            placement,
            fields.primary.dimensions(),
            &mut ids,
            &mut primary_kitty_identities,
            &mut content_ids,
            &mut storage_bytes,
        )?;
    }
    let total_pushed = fields.scrollback.total_pushed();
    let retained_rows = fields.scrollback.len() as u64;
    if retained_rows > total_pushed {
        return Err(ImagePlacementError::HistoryRowsExceedCounter {
            retained_rows,
            total_pushed,
        }
        .to_string());
    }
    let grid_rows = fields.primary.dimensions().0;
    let history_first = total_pushed.saturating_sub(fields.scrollback.len() as u64);
    let live_end = total_pushed
        .checked_add(u64::from(grid_rows))
        .ok_or_else(|| {
            ImagePlacementError::HistoryRangeOverflow {
                total_pushed,
                grid_rows,
            }
            .to_string()
        })?;
    let grid_columns = fields.primary.dimensions().1;
    for placement in &fields.primary_image_history {
        if !matches!(
            placement.record.action,
            ImageAction::Display | ImageAction::TransmitAndDisplay
        ) {
            return Err("image placement image record must be a display record".to_owned());
        }
        if placement.columns == 0 || placement.rows == 0 {
            return Err("image placement dimensions must be nonzero".to_owned());
        }
        if !ids.insert(placement.id) {
            return Err("image placement identities must be unique across screens".to_string());
        }
        if let Some(identity) = kitty_placement_identity(placement.record.as_ref()) {
            if !primary_kitty_identities.insert(identity) {
                return Err(
                    "kitty image and placement identities must be unique per screen".to_string(),
                );
            }
        }
        let history_end = placement
            .anchor
            .0
            .checked_add(u64::from(placement.rows))
            .ok_or_else(|| "image placement row extent overflows u64".to_string())?;
        if placement.anchor.0 >= total_pushed
            || placement.anchor.0 < history_first
            || history_end > live_end
        {
            return Err(ImagePlacementError::HistoryOutOfBounds {
                row: placement.anchor.0,
                column: placement.anchor.1,
                columns: placement.columns,
                rows: placement.rows,
                first_row: history_first,
                retained_end: live_end,
            }
            .to_string());
        }
        if u32::from(placement.anchor.1) + u32::from(placement.columns) > u32::from(grid_columns) {
            return Err(ImagePlacementError::HistoryWidthOutOfBounds {
                row: placement.anchor.0,
                column: placement.anchor.1,
                columns: placement.columns,
                grid_columns,
            }
            .to_string());
        }
        validate_content_storage(&placement.content, &mut content_ids, &mut storage_bytes)?;
        if !Arc::ptr_eq(&placement.record.image, &placement.content.image) {
            return Err("image placement record is not aliased to its content".to_owned());
        }
        validate_plan(
            placement.record.as_ref(),
            &placement.plan,
            placement.columns,
            placement.rows,
        )?;
        if let Some(raster) = &placement.raster {
            validate_pixel_storage(raster, &mut storage_bytes)?;
        }
    }
    let mut alternate_kitty_identities = HashSet::new();
    for placement in &fields.alternate_image_placements {
        validate_live_image_placement(
            placement,
            fields.alternate.dimensions(),
            &mut ids,
            &mut alternate_kitty_identities,
            &mut content_ids,
            &mut storage_bytes,
        )?;
    }
    if content_ids.contains_key(&fields.next_image_content_id) {
        return Err("next image content identity collides with retained content".to_owned());
    }
    if fields.next_image_content_id.get() == u64::MAX {
        return Err("image content identity space is exhausted".to_owned());
    }
    Ok(())
}

fn validate_live_image_placement(
    placement: &ImagePlacement,
    (grid_rows, grid_columns): (u16, u16),
    ids: &mut HashSet<ImagePlacementId>,
    kitty_identities: &mut HashSet<(u32, u32)>,
    content_ids: &mut BTreeMap<ImageContentId, (usize, usize)>,
    storage_bytes: &mut (usize, HashSet<usize>),
) -> Result<(), String> {
    if !matches!(
        placement.record.action,
        ImageAction::Display | ImageAction::TransmitAndDisplay
    ) {
        return Err("image placement image record must be a display record".to_owned());
    }
    if placement.columns == 0 || placement.rows == 0 {
        return Err("image placement dimensions must be nonzero".to_owned());
    }
    if !ids.insert(placement.id) {
        return Err("image placement identities must be unique across screens".to_string());
    }
    if let Some(identity) = kitty_placement_identity(placement.record()) {
        if !kitty_identities.insert(identity) {
            return Err(
                "kitty image and placement identities must be unique per screen".to_string(),
            );
        }
    }
    if !Arc::ptr_eq(&placement.record.image, &placement.content.image) {
        return Err("image placement record is not aliased to its content".to_owned());
    }
    let row_end = u32::from(placement.anchor.0) + u32::from(placement.rows);
    let column_end = u32::from(placement.anchor.1) + u32::from(placement.columns);
    if row_end > u32::from(grid_rows) || column_end > u32::from(grid_columns) {
        return Err(ImagePlacementError::OutOfBounds {
            row: placement.anchor.0,
            column: placement.anchor.1,
            columns: placement.columns,
            rows: placement.rows,
            grid_rows,
            grid_columns,
        }
        .to_string());
    }
    validate_content_storage(&placement.content, content_ids, storage_bytes)?;
    validate_plan(
        placement.record(),
        &placement.plan,
        placement.columns,
        placement.rows,
    )?;
    if let Some(raster) = &placement.raster {
        validate_pixel_storage(raster, storage_bytes)?;
    }
    Ok(())
}

fn validate_content_storage(
    content: &Arc<ImageContent>,
    content_ids: &mut BTreeMap<ImageContentId, (usize, usize)>,
    storage_bytes: &mut (usize, HashSet<usize>),
) -> Result<(), String> {
    let pointer = (
        Arc::as_ptr(content) as usize,
        Arc::as_ptr(&content.image) as usize,
    );
    if let Some(existing) = content_ids.get(&content.id) {
        if *existing != pointer {
            return Err("one image content identity has different pixels".to_owned());
        }
        return Ok(());
    }
    if content_ids.len() == MAX_IMAGE_PLACEMENTS {
        return Err(ImagePlacementError::TooManyPlacements {
            count: MAX_IMAGE_PLACEMENTS + 1,
            limit: MAX_IMAGE_PLACEMENTS,
        }
        .to_string());
    }
    content_ids.insert(content.id, pointer);
    validate_image_pixels(&content.image)?;
    if storage_bytes.1.insert(Arc::as_ptr(&content.image) as usize) {
        add_storage_bytes(content.image.rgba.len(), storage_bytes)?;
    }
    if let Some(animation) = &content.animation {
        validate_animation_content_state(
            &content.image,
            Some(animation),
            content.animation_frame,
            content.animation_loops,
            content.animation_elapsed_nanos,
            content.animation_running,
            content.animation_loading,
        )?;
        for frame in animation.frames() {
            let image = frame.image_shared();
            validate_image_pixels(&image)?;
            if storage_bytes.1.insert(Arc::as_ptr(&image) as usize) {
                add_storage_bytes(frame.image().rgba.len(), storage_bytes)?;
            }
        }
    } else {
        validate_animation_content_state(
            &content.image,
            None,
            content.animation_frame,
            content.animation_loops,
            content.animation_elapsed_nanos,
            content.animation_running,
            content.animation_loading,
        )?;
    }
    if let Some(source) = &content.sixel {
        add_storage_bytes(source.storage_bytes()?, storage_bytes)?;
    }
    Ok(())
}

fn validate_pixel_storage(
    image: &Arc<crate::graphics::DecodedImage>,
    storage_bytes: &mut (usize, HashSet<usize>),
) -> Result<(), String> {
    validate_image_pixels(image)?;
    if !storage_bytes.1.insert(Arc::as_ptr(image) as usize) {
        return Ok(());
    }
    add_storage_bytes(image.rgba.len(), storage_bytes)
}

fn validate_image_pixels(image: &Arc<crate::graphics::DecodedImage>) -> Result<(), String> {
    let width = usize::try_from(image.width)
        .map_err(|_| "image width cannot be represented on this platform".to_owned())?;
    let height = usize::try_from(image.height)
        .map_err(|_| "image height cannot be represented on this platform".to_owned())?;
    let expected = crate::graphics::checked_rgba_len(GraphicsProtocol::Kitty, width, height)
        .map_err(|error| error.to_string())?;
    if image.rgba.len() != expected {
        return Err("image RGBA length does not match its dimensions".to_owned());
    }
    Ok(())
}

fn add_storage_bytes(
    requested_bytes: usize,
    storage_bytes: &mut (usize, HashSet<usize>),
) -> Result<(), String> {
    let used_bytes = storage_bytes.0;
    storage_bytes.0 = used_bytes.checked_add(requested_bytes).ok_or_else(|| {
        ImagePlacementError::StorageLimit {
            used_bytes,
            requested_bytes,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        }
        .to_string()
    })?;
    if storage_bytes.0 > MAX_IMAGE_STORAGE_BYTES {
        return Err(ImagePlacementError::StorageLimit {
            used_bytes,
            requested_bytes,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        }
        .to_string());
    }
    Ok(())
}

const ZERO_FRAME_DELAY_NANOS: u64 = 8_000_000;

struct AdvancedAnimation {
    content: Arc<ImageContent>,
    pixels_changed: bool,
}

impl TerminalState {
    /// Return the time until the next retained animation frame is due.
    pub(crate) fn next_animation_delay(&self) -> Option<std::time::Duration> {
        self.animation_contents()
            .values()
            .filter_map(|content| animation_delay(content).map(nanos_to_duration))
            .min()
    }

    /// Advance every retained animation and report whether visible pixels changed.
    pub(crate) fn advance_animations(&mut self, elapsed: std::time::Duration) -> bool {
        let elapsed_nanos = elapsed.as_nanos().min(u128::from(u64::MAX));
        let replacements = self
            .animation_contents()
            .into_iter()
            .filter_map(|(id, content)| {
                advance_animation_content(&content, elapsed_nanos).map(|advanced| (id, advanced))
            })
            .collect::<BTreeMap<_, _>>();
        if replacements.is_empty() {
            return false;
        }

        let mut pixels_changed = false;
        for placement in &mut self.primary_image_placements {
            if let Some(advanced) = replacements.get(&placement.content.id) {
                pixels_changed |= advanced.pixels_changed;
                apply_animation_content(placement, &advanced.content);
            }
        }
        for placement in &mut self.primary_image_history {
            if let Some(advanced) = replacements.get(&placement.content.id) {
                pixels_changed |= advanced.pixels_changed;
                apply_animation_history_content(placement, &advanced.content);
            }
        }
        for placement in &mut self.alternate_image_placements {
            if let Some(advanced) = replacements.get(&placement.content.id) {
                pixels_changed |= advanced.pixels_changed;
                apply_animation_content(placement, &advanced.content);
            }
        }
        for image in &mut self.kitty_images {
            if let Some(advanced) = replacements.get(&image.content.id) {
                pixels_changed |= advanced.pixels_changed;
                image.content = Arc::clone(&advanced.content);
                let mut record = image.record.as_ref().clone();
                record.image = Arc::clone(&advanced.content.image);
                record.animation = advanced.content.animation.clone();
                image.record = Arc::new(record);
            }
        }
        pixels_changed
    }

    fn animation_contents(&self) -> BTreeMap<ImageContentId, Arc<ImageContent>> {
        let mut contents = BTreeMap::new();
        for placement in &self.primary_image_placements {
            contents
                .entry(placement.content.id)
                .or_insert_with(|| Arc::clone(&placement.content));
        }
        for placement in &self.primary_image_history {
            contents
                .entry(placement.content.id)
                .or_insert_with(|| Arc::clone(&placement.content));
        }
        for placement in &self.alternate_image_placements {
            contents
                .entry(placement.content.id)
                .or_insert_with(|| Arc::clone(&placement.content));
        }
        for image in &self.kitty_images {
            contents
                .entry(image.content.id)
                .or_insert_with(|| Arc::clone(&image.content));
        }
        contents.retain(|_, content| content.animation.is_some());
        contents
    }
}

fn animation_delay(content: &ImageContent) -> Option<u128> {
    if !content.animation_running {
        return None;
    }
    let animation = content.animation.as_ref()?;
    let frame = animation.frames().get(content.animation_frame as usize)?;
    Some(frame_delay_nanos(frame).saturating_sub(u128::from(content.animation_elapsed_nanos)))
}

fn frame_delay_nanos(frame: &koshi_image::AnimationFrame) -> u128 {
    if frame.gapless() {
        return 0;
    }
    let delay = frame.delay();
    let numerator = u128::from(delay.numerator_ms());
    if numerator == 0 {
        return u128::from(ZERO_FRAME_DELAY_NANOS);
    }
    (numerator
        .saturating_mul(1_000_000)
        .saturating_add(u128::from(delay.denominator_ms()).saturating_sub(1)))
        / u128::from(delay.denominator_ms())
}

fn nanos_to_duration(nanos: u128) -> std::time::Duration {
    std::time::Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

fn animation_cycle_nanos(animation: &DecodedAnimation) -> u128 {
    animation.frames().iter().map(frame_delay_nanos).sum()
}

fn advance_animation_content(
    content: &Arc<ImageContent>,
    elapsed_nanos: u128,
) -> Option<AdvancedAnimation> {
    let animation = content.animation.as_ref()?;
    if !content.animation_running {
        return None;
    }
    let frame_count = animation.frame_count();
    let mut frame = usize::try_from(content.animation_frame).ok()?;
    if frame >= frame_count {
        return None;
    }
    let mut loops = content.animation_loops;
    let mut remaining = u128::from(content.animation_elapsed_nanos).saturating_add(elapsed_nanos);
    let old_frame = frame;
    let mut running = true;
    let mut transitions = 0usize;
    let cycle_nanos = animation_cycle_nanos(animation);
    loop {
        if !content.animation_loading && frame == 0 && cycle_nanos != 0 {
            let complete_cycles = remaining / cycle_nanos;
            if complete_cycles != 0 {
                match animation.loop_policy() {
                    koshi_image::LoopPolicy::Infinite => {
                        remaining %= cycle_nanos;
                    }
                    koshi_image::LoopPolicy::Finite(total_playbacks) => {
                        let available_cycles = u128::from(total_playbacks.saturating_sub(loops));
                        if complete_cycles >= available_cycles {
                            frame = last_visible_frame(animation);
                            loops = total_playbacks;
                            remaining = 0;
                            running = false;
                            break;
                        }
                        loops = loops
                            .saturating_add(u32::try_from(complete_cycles).unwrap_or(u32::MAX));
                        remaining %= cycle_nanos;
                    }
                }
            }
        }
        let delay = frame_delay_nanos(&animation.frames()[frame]);
        if remaining < delay {
            break;
        }
        remaining -= delay;
        if delay == 0 {
            transitions = transitions.saturating_add(1);
            if transitions > frame_count {
                frame = last_visible_frame(animation);
                loops = 0;
                remaining = 0;
                running = false;
                break;
            }
        } else {
            transitions = 0;
        }
        if frame + 1 < frame_count {
            frame += 1;
            continue;
        }
        if content.animation_loading {
            frame = last_visible_frame(animation);
            remaining = 0;
            running = false;
            break;
        }
        match animation.loop_policy() {
            koshi_image::LoopPolicy::Infinite => {
                frame = 0;
            }
            koshi_image::LoopPolicy::Finite(total_playbacks) => {
                if loops.saturating_add(1) >= total_playbacks {
                    frame = last_visible_frame(animation);
                    loops = total_playbacks;
                    remaining = 0;
                    running = false;
                    break;
                }
                loops = loops.saturating_add(1);
                frame = 0;
            }
        }
    }

    if frame_count > 1 && animation.frames()[frame].gapless() {
        frame = last_visible_frame(animation);
    }
    let new_elapsed = u64::try_from(remaining).ok()?;
    if frame == old_frame
        && new_elapsed == content.animation_elapsed_nanos
        && running == content.animation_running
    {
        return None;
    }

    let mut next = (**content).clone();
    next.image = animation.frames()[frame].image_shared();
    next.animation_frame = u32::try_from(frame).ok()?;
    next.animation_loops = loops;
    next.animation_elapsed_nanos = new_elapsed;
    next.animation_running = running;
    Some(AdvancedAnimation {
        content: Arc::new(next),
        pixels_changed: frame != old_frame,
    })
}

fn last_visible_frame(animation: &DecodedAnimation) -> usize {
    animation
        .frames()
        .iter()
        .rposition(|frame| !frame.gapless())
        .unwrap_or(0)
}

fn apply_animation_content(placement: &mut ImagePlacement, content: &Arc<ImageContent>) {
    let mut record = placement.record.as_ref().clone();
    record.image = Arc::clone(&content.image);
    record.animation = content.animation.clone();
    let raster = match placement.raster.as_ref() {
        Some(_) => match raster::rebuild(&record, &placement.plan) {
            Ok(raster) => raster,
            Err(_) => return,
        },
        None => None,
    };
    placement.record = Arc::new(record);
    placement.content = Arc::clone(content);
    placement.raster = raster;
}

fn apply_animation_history_content(
    placement: &mut PrimaryHistoryImagePlacement,
    content: &Arc<ImageContent>,
) {
    let mut record = placement.record.as_ref().clone();
    record.image = Arc::clone(&content.image);
    record.animation = content.animation.clone();
    let raster = match placement.raster.as_ref() {
        Some(_) => match raster::rebuild(&record, &placement.plan) {
            Ok(raster) => raster,
            Err(_) => return,
        },
        None => None,
    };
    placement.record = Arc::new(record);
    placement.content = Arc::clone(content);
    placement.raster = raster;
}

impl TerminalState {
    fn take_image_state(&mut self) -> ImageStateSnapshot {
        ImageStateSnapshot {
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

    fn put_image_state(&mut self, state: ImageStateSnapshot) {
        self.primary_image_placements = state.primary_image_placements;
        self.primary_image_history = state.primary_image_history;
        self.alternate_image_placements = state.alternate_image_placements;
        self.kitty_images = state.kitty_images;
        self.next_image_placement_id = state.next_image_placement_id;
        self.next_image_content_id = state.next_image_content_id;
    }

    fn allocate_image_content_id(&mut self) -> Result<ImageContentId, ImagePlacementError> {
        let used = self
            .kitty_images
            .iter()
            .map(|image| image.content.id)
            .chain(
                self.primary_image_placements
                    .iter()
                    .map(|placement| placement.content.id),
            )
            .chain(
                self.primary_image_history
                    .iter()
                    .map(|placement| placement.content.id),
            )
            .chain(
                self.alternate_image_placements
                    .iter()
                    .map(|placement| placement.content.id),
            )
            .collect::<HashSet<_>>();
        let mut candidate = self.next_image_content_id;
        loop {
            let next = candidate
                .get()
                .checked_add(1)
                .and_then(ImageContentId::new)
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            if !used.contains(&candidate) {
                self.next_image_content_id = next;
                return Ok(candidate);
            }
            candidate = next;
        }
    }

    pub(super) fn allocate_image_content(
        &mut self,
        record: Arc<ImageRecord>,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let id = self.allocate_image_content_id()?;
        Ok(Arc::new(ImageContent {
            id,
            image: Arc::clone(&record.image),
            animation: record.animation.clone(),
            animation_frame: 0,
            animation_loops: 0,
            animation_elapsed_nanos: 0,
            animation_running: record.animation.is_some(),
            animation_loading: false,
            sixel: None,
        }))
    }

    pub(super) fn allocate_sixel_image_content(
        &mut self,
        record: Arc<ImageRecord>,
        sixel: SixelImageSource,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        let id = self.allocate_image_content_id()?;
        Ok(Arc::new(ImageContent {
            id,
            image: Arc::clone(&record.image),
            animation: record.animation.clone(),
            animation_frame: 0,
            animation_loops: 0,
            animation_elapsed_nanos: 0,
            animation_running: record.animation.is_some(),
            animation_loading: false,
            sixel: Some(sixel),
        }))
    }

    fn content_for_record(
        &mut self,
        record: &Arc<ImageRecord>,
    ) -> Result<Arc<ImageContent>, ImagePlacementError> {
        if let Some(image_id) = kitty_image_id(record) {
            if let Some(content) = self
                .kitty_images
                .iter()
                .rev()
                .find(|image| {
                    image.display.image_id == Some(image_id)
                        && Arc::ptr_eq(&image.content.image, &record.image)
                })
                .map(|image| Arc::clone(&image.content))
            {
                return Ok(content);
            }
            if let Some(content) = self
                .primary_image_placements
                .iter()
                .chain(&self.alternate_image_placements)
                .map(|placement| (&placement.record, &placement.content))
                .chain(
                    self.primary_image_history
                        .iter()
                        .map(|placement| (&placement.record, &placement.content)),
                )
                .find(|(existing, _)| {
                    kitty_image_id(existing) == Some(image_id)
                        && Arc::ptr_eq(&existing.image, &record.image)
                })
                .map(|(_, content)| Arc::clone(content))
            {
                return Ok(content);
            }
        }
        self.allocate_image_content(Arc::clone(record))
    }

    /// Return image placements whose anchors are on the active live screen in
    /// insertion order. Primary placements in retained history are returned by
    /// [`image_placements_for_view`](Self::image_placements_for_view).
    #[must_use]
    pub fn image_placements(&self) -> &[ImagePlacement] {
        match self.active {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    /// Return the image portions visible at the requested scrollback offset.
    /// The complete image scale is retained when a view edge clips its cells.
    #[must_use]
    pub fn image_placements_for_view(&self, offset: usize) -> Vec<ImagePlacement> {
        if self.active == Screen::Alternate {
            let mut virtual_placements = self.virtual_image_placements(self.alternate.as_ref(), 0);
            let mut absolute = self
                .alternate_image_placements
                .iter()
                .cloned()
                .map(|placement| AbsoluteImagePlacement {
                    id: placement.id,
                    record: placement.record,
                    content: placement.content,
                    anchor: (u64::from(placement.anchor.0), placement.anchor.1),
                    columns: placement.columns,
                    rows: placement.rows,
                    plan: placement.plan,
                    raster: placement.raster,
                })
                .collect::<Vec<_>>();
            self.remap_relative_absolute_placements(&mut absolute, &virtual_placements);
            absolute.append(&mut virtual_placements);
            let mut placements = absolute
                .into_iter()
                .filter_map(|placement| placement.into_live(0))
                .collect::<Vec<_>>();
            placements.sort_unstable_by_key(ImagePlacement::id);
            return placements;
        }
        let (grid, scrolled) = self.scrolled_view(offset);
        let (rows, columns) = grid.dimensions();
        let top = self
            .scrollback
            .total_pushed()
            .saturating_sub(scrolled as u64);
        let end = top.saturating_add(u64::from(rows));
        let mut placements = self
            .primary_absolute_image_placements()
            .into_iter()
            .collect::<Vec<_>>();
        let mut virtual_placements = self.virtual_image_placements(grid.as_ref(), top);
        self.remap_relative_absolute_placements(&mut placements, &virtual_placements);
        placements.append(&mut virtual_placements);
        placements
            .into_iter()
            .filter_map(|placement| placement.clipped(top, end, columns)?.into_live(top))
            .collect::<Vec<_>>()
    }

    fn virtual_image_placements(
        &self,
        grid: &Grid,
        row_origin: u64,
    ) -> Vec<AbsoluteImagePlacement> {
        self.kitty_images
            .iter()
            .filter(|image| image.virtual_placement && image.virtual_screen == Some(self.active))
            .filter_map(|image| {
                let (row, column) = placeholder_anchor(grid, image)?;
                let columns = image.display.cell_columns?;
                let rows = image.display.cell_rows?;
                let mut record = image.record.as_ref().clone();
                record.display.unicode_placeholder = false;
                record.display.move_cursor = false;
                record.anchor = (row, column);
                let prepared =
                    raster::prepare_with_plan(&record, self.cell_size, grid.dimensions()).ok()?;
                let columns = u16::try_from(columns).ok()?;
                let rows = u16::try_from(rows).ok()?;
                if prepared.columns != u32::from(columns) || prepared.rows != u32::from(rows) {
                    return None;
                }
                let id = virtual_image_placement_id(&record, row, column);
                Some(AbsoluteImagePlacement {
                    id,
                    record: Arc::new(record),
                    content: Arc::clone(&image.content),
                    anchor: (row_origin.checked_add(u64::from(row))?, column),
                    columns,
                    rows,
                    plan: prepared.plan,
                    raster: prepared.raster,
                })
            })
            .collect()
    }

    fn remap_relative_absolute_placements(
        &self,
        placements: &mut [AbsoluteImagePlacement],
        virtual_placements: &[AbsoluteImagePlacement],
    ) {
        let mut entries = virtual_placements
            .iter()
            .map(|placement| {
                (
                    Arc::clone(&placement.record),
                    (
                        i64::try_from(placement.anchor.0).unwrap_or(i64::MAX),
                        i64::from(placement.anchor.1),
                    ),
                )
            })
            .chain(placements.iter().map(|placement| {
                (
                    Arc::clone(&placement.record),
                    (
                        i64::try_from(placement.anchor.0).unwrap_or(i64::MAX),
                        i64::from(placement.anchor.1),
                    ),
                )
            }))
            .collect::<Vec<_>>();
        let virtual_count = virtual_placements.len();
        for index in virtual_count..entries.len() {
            let Some(anchor) =
                resolve_view_relative_anchor(index, &entries, &mut HashSet::new(), 0)
            else {
                continue;
            };
            entries[index].1 = anchor;
            if let (Ok(row), Ok(column)) = (u64::try_from(anchor.0), u16::try_from(anchor.1)) {
                placements[index - virtual_count].anchor = (row, column);
            }
        }
    }

    pub(super) fn primary_absolute_image_placements(&self) -> Vec<AbsoluteImagePlacement> {
        self.primary_absolute_image_placements_at(self.scrollback.total_pushed())
    }

    fn primary_absolute_image_placements_at(&self, live_top: u64) -> Vec<AbsoluteImagePlacement> {
        let mut placements = self
            .primary_image_history
            .iter()
            .cloned()
            .map(PrimaryHistoryImagePlacement::into_absolute)
            .collect::<Vec<_>>();
        placements.extend(
            self.primary_image_placements
                .iter()
                .cloned()
                .filter_map(|placement| AbsoluteImagePlacement::from_live(placement, live_top)),
        );
        placements
    }

    pub(super) fn remap_primary_image_placements<F>(&mut self, old_live_top: u64, mut map: F)
    where
        F: FnMut(u64, u16) -> Option<(u64, u16)>,
    {
        let mut placements = self
            .primary_absolute_image_placements_at(old_live_top)
            .into_iter()
            .filter_map(|mut placement| {
                let (removed, mapped_anchor) = (0..placement.rows).find_map(|removed| {
                    let old_row = placement.anchor.0.checked_add(u64::from(removed))?;
                    map(old_row, placement.anchor.1).map(|anchor| (removed, anchor))
                })?;
                placement.plan.geometry.offset.y =
                    placement.plan.geometry.offset.y.checked_add(removed)?;
                placement.rows -= removed;
                Some(placement.with_anchor(mapped_anchor))
            })
            .collect::<Vec<_>>();
        self.set_primary_absolute_image_placements(&mut placements);
    }

    pub(super) fn remap_alternate_image_placements<F>(&mut self, mut map: F)
    where
        F: FnMut(u16, u16) -> Option<(u16, u16)>,
    {
        let (rows, columns) = self.alternate.dimensions();
        self.alternate_image_placements = std::mem::take(&mut self.alternate_image_placements)
            .into_iter()
            .filter_map(|mut placement| {
                let (removed, anchor) = (0..placement.rows).find_map(|removed| {
                    map(placement.anchor.0.checked_add(removed)?, placement.anchor.1)
                        .map(|anchor| (removed, anchor))
                })?;
                placement.plan.geometry.offset.y =
                    placement.plan.geometry.offset.y.checked_add(removed)?;
                placement.rows -= removed;
                let placement = placement.with_anchor(anchor);
                AbsoluteImagePlacement::from_live(placement, 0)?
                    .clipped(0, u64::from(rows), columns)?
                    .into_live(0)
            })
            .collect();
    }

    fn set_primary_absolute_image_placements(
        &mut self,
        placements: &mut Vec<AbsoluteImagePlacement>,
    ) {
        placements.sort_unstable_by_key(|placement| placement.id);
        self.primary_image_placements.clear();
        self.primary_image_history.clear();

        let history_len = self.scrollback.len() as u64;
        let live_top = self.scrollback.total_pushed();
        let oldest_history_row = live_top.saturating_sub(history_len);
        let live_end = live_top.saturating_add(u64::from(self.primary.dimensions().0));
        let grid_columns = self.primary.dimensions().1;

        for placement in placements.drain(..) {
            let Some(placement) = placement.clipped(oldest_history_row, live_end, grid_columns)
            else {
                continue;
            };
            if placement.anchor.0 < live_top {
                self.primary_image_history
                    .push(PrimaryHistoryImagePlacement::from_absolute(placement));
            } else if let Some(placement) = placement.into_live(live_top) {
                self.primary_image_placements.push(placement);
            }
        }
    }

    /// Apply one decoded image record to terminal image state.
    pub(crate) fn apply_image_record(
        &mut self,
        record: &ImageRecord,
    ) -> Result<(), ImagePlacementError> {
        self.apply_image_record_with_sixel_options(record, None, false, None)
    }

    pub(crate) fn apply_sixel_image_record(
        &mut self,
        record: &ImageRecord,
        sixel_scrolling: bool,
        sixel_cursor_right: bool,
        sixel_source: SixelImageSource,
    ) -> Result<(), ImagePlacementError> {
        let original = self.clone();
        let result = self.apply_image_record_with_sixel_options(
            record,
            Some(sixel_scrolling),
            sixel_cursor_right,
            Some(sixel_source),
        );
        if result.is_err() {
            *self = original;
        }
        result
    }

    fn apply_image_record_with_sixel_options(
        &mut self,
        record: &ImageRecord,
        sixel_scrolling: Option<bool>,
        sixel_cursor_right: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(), ImagePlacementError> {
        let original = self.take_image_state();
        let staged = original.clone();
        self.put_image_state(staged);
        let result = self.apply_image_record_inner(
            record,
            sixel_scrolling,
            sixel_cursor_right,
            sixel_source,
        );
        let candidate = self.take_image_state();
        match result {
            Ok((_display, cursor_movement)) => {
                self.put_image_state(candidate);
                if let Some(movement) = cursor_movement {
                    self.apply_image_cursor_movement(movement);
                }
                Ok(())
            }
            Err(error) => {
                self.put_image_state(original);
                Err(error)
            }
        }
    }

    fn apply_image_record_inner(
        &mut self,
        record: &ImageRecord,
        sixel_scrolling: Option<bool>,
        sixel_cursor_right: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(crate::graphics::ImageDisplay, Option<ImageCursorMovement>), ImagePlacementError>
    {
        let mut record = record.clone();
        self.retain_kitty_upload(&mut record)?;
        let relative = record.display.relative_image_id.is_some()
            || record.display.relative_placement_id.is_some();
        let move_cursor = record.display.move_cursor && !relative;
        let movement = match record.action {
            ImageAction::Transmit => None,
            ImageAction::Display | ImageAction::TransmitAndDisplay => {
                let (columns, rows) =
                    self.place_image(&mut record, sixel_scrolling.unwrap_or(false), sixel_source)?;
                Some(if sixel_scrolling.is_some() {
                    ImageCursorMovement::Sixel {
                        anchor: record.anchor,
                        columns,
                        rows,
                        cursor_right: sixel_cursor_right,
                    }
                } else {
                    ImageCursorMovement::Regular { columns, rows }
                })
            }
        };
        self.check_image_storage(record.image.rgba.len(), kitty_image_id(&record))?;
        Ok((record.display, movement.filter(|_| move_cursor)))
    }

    fn place_image(
        &mut self,
        record: &mut ImageRecord,
        sixel_scrolling: bool,
        sixel_source: Option<SixelImageSource>,
    ) -> Result<(u16, u16), ImagePlacementError> {
        if record.display.unicode_placeholder {
            return Err(ImagePlacementError::UnsupportedPlacement);
        }
        if let Some(anchor) = self.resolve_relative_anchor(record)? {
            record.anchor = anchor;
            record.display.move_cursor = false;
        }
        record.source_rect()?;
        let prepared =
            raster::prepare_with_plan(record, self.cell_size, self.active_grid().dimensions())?;
        let columns = prepared.columns;
        let rows = prepared.rows;
        let columns = u16::try_from(columns)
            .map_err(|_| ImagePlacementError::DimensionsTooLarge { columns, rows })?;
        let rows = u16::try_from(rows).map_err(|_| ImagePlacementError::DimensionsTooLarge {
            columns: u32::from(columns),
            rows,
        })?;
        if columns == 0 || rows == 0 {
            return Err(ImagePlacementError::ZeroSize {
                columns: u32::from(columns),
                rows: u32::from(rows),
            });
        }

        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        if record.anchor.0 >= grid_rows || record.anchor.1 >= grid_columns {
            return Err(ImagePlacementError::OutOfBounds {
                row: record.anchor.0,
                column: record.anchor.1,
                columns,
                rows,
                grid_rows,
                grid_columns,
            });
        }

        let replacement_slot = kitty_placement_identity(record)
            .and_then(|identity| self.active_image_placement_slot(identity));
        let count = self.primary_image_placements.len()
            + self.primary_image_history.len()
            + self.alternate_image_placements.len()
            + usize::from(replacement_slot.is_none());
        if count > MAX_IMAGE_PLACEMENTS {
            return Err(ImagePlacementError::TooManyPlacements {
                count,
                limit: MAX_IMAGE_PLACEMENTS,
            });
        }
        if let Some(source) = &sixel_source {
            let raster_bytes = prepared
                .raster
                .as_ref()
                .map_or(0, |raster| raster.rgba.len());
            let source_bytes =
                source
                    .storage_bytes()
                    .map_err(|_| ImagePlacementError::StorageLimit {
                        used_bytes: 0,
                        requested_bytes: usize::MAX,
                        limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                    })?;
            let requested_bytes = record
                .image
                .rgba
                .len()
                .checked_add(raster_bytes)
                .and_then(|bytes| bytes.checked_add(source_bytes))
                .ok_or(ImagePlacementError::StorageLimit {
                    used_bytes: 0,
                    requested_bytes: usize::MAX,
                    limit_bytes: MAX_IMAGE_STORAGE_BYTES,
                })?;
            self.reserve_sixel_storage(requested_bytes)?;
        }
        let id = if let Some(slot) = replacement_slot {
            match slot {
                ActiveImagePlacementSlot::Live(index) => self.active_image_placements()[index].id,
                ActiveImagePlacementSlot::History(index) => self.primary_image_history[index].id,
            }
        } else {
            self.allocate_image_placement_id()?
        };
        let content_record = Arc::new(record.clone());
        let content = if let Some(sixel) = sixel_source {
            self.allocate_sixel_image_content(Arc::clone(&content_record), sixel)?
        } else {
            self.content_for_record(&content_record)?
        };
        if sixel_scrolling {
            self.scroll_sixel_to_fit(&mut record.anchor, rows);
        }
        let record = Arc::new(record.clone());
        let mut placement = ImagePlacement::new(
            id,
            Arc::clone(&record),
            content,
            prepared.plan,
            columns,
            rows,
            prepared.raster,
        );
        placement.anchor = record.anchor;
        if placement.raster.is_some() {
            let retained = self
                .primary_image_placements
                .iter()
                .chain(&self.alternate_image_placements)
                .find_map(|existing| {
                    (Arc::ptr_eq(&existing.content, &placement.content)
                        && existing.plan.same_raster(&placement.plan))
                    .then(|| existing.raster.clone())
                    .flatten()
                })
                .or_else(|| {
                    self.primary_image_history.iter().find_map(|existing| {
                        (Arc::ptr_eq(&existing.content, &placement.content)
                            && existing.plan.same_raster(&placement.plan))
                        .then(|| existing.raster.clone())
                        .flatten()
                    })
                });
            if let Some(retained) = retained {
                placement.raster = Some(Arc::clone(&retained));
            }
        }
        placement.rows = rows.min(grid_rows - record.anchor.0);
        placement.columns = columns.min(grid_columns - record.anchor.1);
        match replacement_slot {
            Some(ActiveImagePlacementSlot::Live(index)) => {
                self.active_image_placements_mut()[index] = placement;
            }
            Some(ActiveImagePlacementSlot::History(index)) => {
                self.primary_image_history.remove(index);
                self.insert_active_image_placement_in_order(placement);
            }
            None => self.active_image_placements_mut().push(placement),
        }

        Ok((columns, rows))
    }

    fn scroll_sixel_to_fit(&mut self, anchor: &mut (u16, u16), rows: u16) {
        let (grid_rows, _) = self.active_grid().dimensions();
        let (top, bottom) =
            self.scroll_region()
                .map_or((0, grid_rows.saturating_sub(1)), |(top, bottom)| {
                    (
                        top.min(grid_rows.saturating_sub(1)),
                        bottom.min(grid_rows.saturating_sub(1)),
                    )
                });
        if top > bottom || anchor.0 < top {
            return;
        }
        let image_end = u32::from(anchor.0).saturating_add(u32::from(rows));
        let region_end = u32::from(bottom) + 1;
        let overflow = image_end.saturating_sub(region_end);
        let shift = overflow.min(u32::from(anchor.0 - top));
        let Ok(shift) = u16::try_from(shift) else {
            return;
        };
        if shift == 0 {
            return;
        }
        let fill = self.active_render().style.bg_fill();
        self.delete_lines_into_scrollback(top, bottom, shift, fill);
        anchor.0 -= shift;
    }

    pub(super) fn refresh_shared_sixel_images(
        &mut self,
        palette: &SixelPalette,
    ) -> Result<(), crate::graphics::GraphicsError> {
        let mut contents = HashMap::new();
        for placement in &mut self.primary_image_placements {
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
        let mut candidate = self.next_image_placement_id;
        loop {
            if candidate == 0 {
                return Err(ImagePlacementError::IdentityExhausted);
            }
            let next = candidate
                .checked_add(1)
                .ok_or(ImagePlacementError::IdentityExhausted)?;
            let used = self
                .primary_image_placements
                .iter()
                .map(|placement| placement.id)
                .chain(
                    self.primary_image_history
                        .iter()
                        .map(|placement| placement.id),
                )
                .chain(
                    self.alternate_image_placements
                        .iter()
                        .map(|placement| placement.id),
                )
                .any(|id| id == candidate);
            if !used {
                self.next_image_placement_id = next;
                return Ok(candidate);
            }
            candidate = next;
        }
    }

    fn active_image_placements_mut(&mut self) -> &mut Vec<ImagePlacement> {
        match self.active {
            Screen::Primary => &mut self.primary_image_placements,
            Screen::Alternate => &mut self.alternate_image_placements,
        }
    }

    fn active_image_placements(&self) -> &[ImagePlacement] {
        match self.active {
            Screen::Primary => &self.primary_image_placements,
            Screen::Alternate => &self.alternate_image_placements,
        }
    }

    fn insert_active_image_placement_in_order(&mut self, placement: ImagePlacement) {
        let placements = self.active_image_placements_mut();
        let index = placements
            .iter()
            .position(|existing| existing.id > placement.id)
            .unwrap_or(placements.len());
        placements.insert(index, placement);
    }

    fn active_image_placement_slot(
        &self,
        identity: (u32, u32),
    ) -> Option<ActiveImagePlacementSlot> {
        match self.active {
            Screen::Primary => self
                .primary_image_placements
                .iter()
                .position(|placement| {
                    kitty_placement_identity(placement.record()) == Some(identity)
                })
                .map(ActiveImagePlacementSlot::Live)
                .or_else(|| {
                    self.primary_image_history
                        .iter()
                        .position(|placement| {
                            kitty_placement_identity(placement.record.as_ref()) == Some(identity)
                        })
                        .map(ActiveImagePlacementSlot::History)
                }),
            Screen::Alternate => self
                .alternate_image_placements
                .iter()
                .position(|placement| {
                    kitty_placement_identity(placement.record()) == Some(identity)
                })
                .map(ActiveImagePlacementSlot::Live),
        }
    }

    fn remove_kitty_image(&mut self, image_id: u32) {
        self.primary_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
        self.primary_image_history.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
        self.alternate_image_placements.retain(|placement| {
            placement.record.protocol != GraphicsProtocol::Kitty
                || placement.record.display.image_id != Some(image_id)
        });
    }

    pub(super) fn clear_active_image_placements(&mut self) {
        match self.active {
            Screen::Primary => {
                let live_top = self.scrollback.total_pushed();
                self.primary_image_placements.clear();
                self.primary_image_history.retain(|placement| {
                    placement
                        .anchor
                        .0
                        .checked_add(u64::from(placement.rows))
                        .is_some_and(|end| end <= live_top)
                });
            }
            Screen::Alternate => self.alternate_image_placements.clear(),
        }
    }

    pub(super) fn clear_alternate_image_placements(&mut self) {
        self.alternate_image_placements.clear();
    }

    pub(super) fn clear_primary_image_history(&mut self) {
        self.primary_image_history.clear();
    }

    pub(super) fn clear_all_image_placements(&mut self) {
        self.kitty_images.clear();
        self.primary_image_placements.clear();
        self.primary_image_history.clear();
        self.alternate_image_placements.clear();
    }

    pub(super) fn clear_images_at_cells(&mut self, row: u16, column: u16, columns: u16) {
        if columns == 0 {
            return;
        }
        let row = u64::from(row);
        let column = u32::from(column);
        let columns = u32::from(columns);
        match self.active {
            Screen::Primary => {
                let live_top = self.scrollback.total_pushed();
                let Some(absolute_row) = live_top.checked_add(row) else {
                    return;
                };
                self.primary_image_placements.retain(|placement| {
                    !image_covers_cells(placement, absolute_row, column, columns, live_top)
                });
                self.primary_image_history.retain(|placement| {
                    !image_covers_cells(placement, absolute_row, column, columns, 0)
                });
            }
            Screen::Alternate => {
                self.alternate_image_placements
                    .retain(|placement| !image_covers_cells(placement, row, column, columns, 0));
            }
        }
    }

    fn apply_image_cursor_movement(&mut self, movement: ImageCursorMovement) {
        match movement {
            ImageCursorMovement::Regular { columns, rows } => {
                self.move_cursor_after_image(columns, rows);
            }
            ImageCursorMovement::Sixel {
                anchor,
                columns,
                rows,
                cursor_right,
            } => self.move_cursor_after_sixel(anchor, columns, rows, cursor_right),
        }
    }

    fn move_cursor_after_image(&mut self, columns: u16, rows: u16) {
        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        let cursor = self.active_cursor_mut();
        cursor.row = cursor
            .row
            .saturating_add(rows)
            .min(grid_rows.saturating_sub(1));
        cursor.col = cursor
            .col
            .saturating_add(columns)
            .min(grid_columns.saturating_sub(1));
        cursor.pending_wrap = false;
    }

    fn move_cursor_after_sixel(
        &mut self,
        anchor: (u16, u16),
        columns: u16,
        rows: u16,
        cursor_right: bool,
    ) {
        let (grid_rows, grid_columns) = self.active_grid().dimensions();
        if grid_rows == 0 || grid_columns == 0 {
            return;
        }
        let (top, bottom) = self
            .scroll_region()
            .map_or((0, grid_rows - 1), |(top, bottom)| {
                (top.min(grid_rows - 1), bottom.min(grid_rows - 1))
            });
        let mut row = anchor
            .0
            .saturating_add(rows.saturating_sub(1))
            .min(grid_rows - 1);
        let mut col = anchor.1;
        if cursor_right {
            let right_edge = u32::from(anchor.1).saturating_add(u32::from(columns));
            if right_edge >= u32::from(grid_columns) {
                col = 0;
                row = row.saturating_add(1);
            } else {
                col = u16::try_from(right_edge).unwrap_or(grid_columns - 1);
            }
        }
        if top <= bottom {
            let fill = self.active_render().style.bg_fill();
            while row > bottom {
                self.delete_lines_into_scrollback(top, bottom, 1, fill);
                row -= 1;
            }
        }
        let cursor = self.active_cursor_mut();
        cursor.row = row.min(grid_rows - 1);
        cursor.col = col.min(grid_columns - 1);
        cursor.pending_wrap = false;
    }
}

fn resolve_view_relative_anchor(
    index: usize,
    entries: &[(Arc<ImageRecord>, (i64, i64))],
    visiting: &mut HashSet<usize>,
    depth: usize,
) -> Option<(i64, i64)> {
    let (record, anchor) = entries.get(index)?;
    if depth > MAX_RELATIVE_PLACEMENT_DEPTH || !visiting.insert(index) {
        return None;
    }
    let result = if let Some(image_id) = record.display.relative_image_id {
        let placement_id = record
            .display
            .relative_placement_id
            .filter(|placement_id| *placement_id != 0);
        let parent = (0..index).rev().find(|candidate| {
            let parent = &entries[*candidate].0;
            parent.protocol == GraphicsProtocol::Kitty
                && parent.display.image_id == Some(image_id)
                && placement_id
                    .is_none_or(|placement_id| parent.display.placement_id == Some(placement_id))
        })?;
        let (parent_row, parent_column) =
            resolve_view_relative_anchor(parent, entries, visiting, depth + 1)?;
        Some((
            parent_row.checked_add(i64::from(record.display.relative_offset_y))?,
            parent_column.checked_add(i64::from(record.display.relative_offset_x))?,
        ))
    } else if record.display.relative_placement_id.is_some()
        || record.display.relative_offset_x != 0
        || record.display.relative_offset_y != 0
    {
        None
    } else {
        Some(*anchor)
    };
    visiting.remove(&index);
    result
}

fn refresh_shared_sixel_placement(
    placement: &mut ImagePlacement,
    palette: &SixelPalette,
    contents: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<(), crate::graphics::GraphicsError> {
    let Some(content) = refreshed_shared_sixel_content(&placement.content, palette, contents)?
    else {
        return Ok(());
    };
    let mut record = placement.record.as_ref().clone();
    record.image = Arc::clone(&content.image);
    let record = Arc::new(record);
    let raster = raster::rebuild(&record, &placement.plan).map_err(|reason| {
        crate::graphics::GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            reason,
        }
    })?;
    placement.record = record;
    placement.content = content;
    placement.raster = raster;
    Ok(())
}

fn refresh_shared_sixel_history_placement(
    placement: &mut PrimaryHistoryImagePlacement,
    palette: &SixelPalette,
    contents: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<(), crate::graphics::GraphicsError> {
    let Some(content) = refreshed_shared_sixel_content(&placement.content, palette, contents)?
    else {
        return Ok(());
    };
    let mut record = placement.record.as_ref().clone();
    record.image = Arc::clone(&content.image);
    let record = Arc::new(record);
    let raster = raster::rebuild(&record, &placement.plan).map_err(|reason| {
        crate::graphics::GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            reason,
        }
    })?;
    placement.record = record;
    placement.content = content;
    placement.raster = raster;
    Ok(())
}

fn refreshed_shared_sixel_content(
    content: &Arc<ImageContent>,
    palette: &SixelPalette,
    contents: &mut HashMap<ImageContentId, Arc<ImageContent>>,
) -> Result<Option<Arc<ImageContent>>, crate::graphics::GraphicsError> {
    let Some(source) = content
        .sixel
        .as_ref()
        .filter(|source| source.shared_palette)
    else {
        return Ok(None);
    };
    if let Some(content) = contents.get(&content.id) {
        return Ok(Some(Arc::clone(content)));
    }
    let source = source.with_shared_palette(palette);
    let image = source.resolved(palette)?;
    let content = Arc::new(ImageContent {
        id: content.id,
        image,
        animation: content.animation.clone(),
        animation_frame: content.animation_frame,
        animation_loops: content.animation_loops,
        animation_elapsed_nanos: content.animation_elapsed_nanos,
        animation_running: content.animation_running,
        animation_loading: content.animation_loading,
        sixel: Some(source),
    });
    contents.insert(content.id, Arc::clone(&content));
    Ok(Some(content))
}

fn image_covers_cells(
    placement: &impl ImageCellPlacement,
    row: u64,
    column: u32,
    columns: u32,
    live_top: u64,
) -> bool {
    let (anchor_row, anchor_column) = placement.image_anchor();
    let anchor_row = anchor_row.saturating_add(live_top);
    let row_end = anchor_row.saturating_add(u64::from(placement.image_rows()));
    let column_end = u32::from(anchor_column).saturating_add(u32::from(placement.image_columns()));
    row >= anchor_row
        && row < row_end
        && column < column_end
        && column.saturating_add(columns) > u32::from(anchor_column)
}

#[derive(Clone, Copy)]
struct ResolvedPlaceholder {
    image_id: u32,
    placement_id: Option<u32>,
    row: u16,
    column: u16,
    image_id_msb: u8,
}

fn placeholder_anchor(grid: &Grid, image: &KittyImage) -> Option<(u16, u16)> {
    let image_id = image.display.image_id.filter(|id| *id != 0)?;
    let mut anchor = None;
    for (row, cells) in grid.rows().iter().enumerate() {
        let mut previous = None;
        for (column, cell) in cells.iter().enumerate() {
            let Some(raw) = cell.image_placeholder() else {
                previous = None;
                continue;
            };
            let Some(placeholder) = resolve_placeholder(raw, previous) else {
                previous = None;
                continue;
            };
            previous = Some(placeholder);
            let resolved_image_id =
                placeholder.image_id | (u32::from(placeholder.image_id_msb) << 24);
            if resolved_image_id != image_id {
                continue;
            }
            if placeholder.placement_id.is_some()
                && placeholder.placement_id != image.display.placement_id
            {
                continue;
            }
            let source_row = u16::try_from(row).ok()?;
            let source_column = u16::try_from(column).ok()?;
            let top = source_row.checked_sub(placeholder.row)?;
            let left = source_column.checked_sub(placeholder.column)?;
            anchor =
                Some(anchor.map_or((top, left), |current: (u16, u16)| current.min((top, left))));
        }
    }
    anchor
}

fn resolve_placeholder(
    raw: ImagePlaceholder,
    previous: Option<ResolvedPlaceholder>,
) -> Option<ResolvedPlaceholder> {
    let same_identity = previous.is_some_and(|previous| {
        previous.image_id == raw.image_id && previous.placement_id == raw.placement_id
    });
    let row = raw.row.or_else(|| same_identity.then_some(previous?.row))?;
    let column = match raw.column {
        Some(column) => column,
        None if same_identity && previous?.row == row => previous?.column.checked_add(1)?,
        None => return None,
    };
    let image_id_msb = raw
        .image_id_msb
        .or_else(|| same_identity.then_some(previous?.image_id_msb))
        .unwrap_or(0);
    Some(ResolvedPlaceholder {
        image_id: raw.image_id,
        placement_id: raw.placement_id,
        row,
        column,
        image_id_msb,
    })
}

fn virtual_image_placement_id(record: &ImageRecord, row: u16, column: u16) -> ImagePlacementId {
    let image_id = u64::from(record.display.image_id.unwrap_or_default());
    let placement_id = u64::from(record.display.placement_id.unwrap_or_default());
    let value = (image_id << 32) ^ (placement_id << 16) ^ (u64::from(row) << 8) ^ u64::from(column);
    value | (1u64 << 63)
}

trait ImageCellPlacement {
    fn image_anchor(&self) -> (u64, u16);

    fn image_columns(&self) -> u16;

    fn image_rows(&self) -> u16;
}

impl ImageCellPlacement for ImagePlacement {
    fn image_anchor(&self) -> (u64, u16) {
        (u64::from(self.anchor.0), self.anchor.1)
    }

    fn image_columns(&self) -> u16 {
        self.columns
    }

    fn image_rows(&self) -> u16 {
        self.rows
    }
}

impl ImageCellPlacement for PrimaryHistoryImagePlacement {
    fn image_anchor(&self) -> (u64, u16) {
        self.anchor
    }

    fn image_columns(&self) -> u16 {
        self.columns
    }

    fn image_rows(&self) -> u16 {
        self.rows
    }
}

fn kitty_placement_identity(record: &ImageRecord) -> Option<(u32, u32)> {
    (record.protocol == GraphicsProtocol::Kitty)
        .then_some((record.display.image_id?, record.display.placement_id?))
        .filter(|(image_id, placement_id)| *image_id != 0 && *placement_id != 0)
}

fn kitty_image_id(record: &ImageRecord) -> Option<u32> {
    (record.protocol == GraphicsProtocol::Kitty)
        .then_some(record.display.image_id)
        .flatten()
        .filter(|image_id| *image_id != 0)
}

const MAX_RELATIVE_PLACEMENT_DEPTH: usize = 8;

#[derive(Clone)]
struct RelativeParent {
    record: Arc<ImageRecord>,
    anchor: (i64, i64),
}

impl TerminalState {
    fn resolve_relative_anchor(
        &self,
        record: &ImageRecord,
    ) -> Result<Option<(u16, u16)>, ImagePlacementError> {
        let display = &record.display;
        let has_parent_fields =
            display.relative_image_id.is_some() || display.relative_placement_id.is_some();
        if !has_parent_fields {
            if display.relative_offset_x != 0 || display.relative_offset_y != 0 {
                return Err(ImagePlacementError::NoParent);
            }
            return Ok(None);
        }
        let Some(parent_id) = display.relative_image_id.filter(|id| *id != 0) else {
            return Err(ImagePlacementError::NoParent);
        };
        let parent_placement_id = display
            .relative_placement_id
            .filter(|placement_id| *placement_id != 0);
        let mut parent = self
            .relative_parent(parent_id, parent_placement_id)
            .ok_or(ImagePlacementError::NoParent)?;
        let mut row = parent.anchor.0;
        let mut column = parent.anchor.1;
        let mut seen = HashSet::new();
        if let Some(identity) = kitty_placement_identity(record) {
            seen.insert((identity.0, Some(identity.1)));
        }
        for depth in 0..=MAX_RELATIVE_PLACEMENT_DEPTH {
            let parent_identity = (
                parent
                    .record
                    .display
                    .image_id
                    .ok_or(ImagePlacementError::NoParent)?,
                parent.record.display.placement_id.filter(|id| *id != 0),
            );
            if !seen.insert(parent_identity) {
                return Err(ImagePlacementError::RelativeCycle);
            }
            let parent_display = &parent.record.display;
            let Some(next_parent_id) = parent_display.relative_image_id else {
                if parent_display.relative_placement_id.is_some()
                    || parent_display.relative_offset_x != 0
                    || parent_display.relative_offset_y != 0
                {
                    return Err(ImagePlacementError::NoParent);
                }
                break;
            };
            if depth == MAX_RELATIVE_PLACEMENT_DEPTH {
                return Err(ImagePlacementError::RelativeDepth);
            }
            row = row
                .checked_add(i64::from(parent_display.relative_offset_y))
                .ok_or(ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
            column = column
                .checked_add(i64::from(parent_display.relative_offset_x))
                .ok_or(ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
            parent = self
                .relative_parent(
                    next_parent_id,
                    parent_display
                        .relative_placement_id
                        .filter(|placement_id| *placement_id != 0),
                )
                .ok_or(ImagePlacementError::NoParent)?;
        }
        row = row
            .checked_add(i64::from(display.relative_offset_y))
            .ok_or(ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
        column = column
            .checked_add(i64::from(display.relative_offset_x))
            .ok_or(ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
        let resolved_row = u16::try_from(row)
            .map_err(|_| ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
        let resolved_column = u16::try_from(column)
            .map_err(|_| ImagePlacementError::RelativeOffsetOutOfBounds { row, column })?;
        Ok(Some((resolved_row, resolved_column)))
    }

    fn relative_parent(&self, image_id: u32, placement_id: Option<u32>) -> Option<RelativeParent> {
        let matches = |record: &ImageRecord| {
            record.protocol == GraphicsProtocol::Kitty
                && record.display.image_id == Some(image_id)
                && placement_id
                    .is_none_or(|placement_id| record.display.placement_id == Some(placement_id))
        };
        let live = self
            .active_image_placements()
            .iter()
            .rev()
            .find(|placement| matches(placement.record()));
        if let Some(placement) = live {
            return Some(RelativeParent {
                record: placement.record_arc(),
                anchor: (i64::from(placement.anchor.0), i64::from(placement.anchor.1)),
            });
        }
        if self.active == Screen::Alternate {
            return None;
        }
        let live_top = self.scrollback.total_pushed();
        if let Some(placement) = self.primary_image_history.iter().rev().find(|placement| {
            matches(placement.record.as_ref())
                && placement.anchor.0 >= live_top
                && placement.anchor.0 - live_top <= u64::from(u16::MAX)
        }) {
            return Some(RelativeParent {
                record: Arc::clone(&placement.record),
                anchor: (
                    i64::try_from(placement.anchor.0 - live_top).ok()?,
                    i64::from(placement.anchor.1),
                ),
            });
        }
        self.kitty_images
            .iter()
            .rev()
            .filter(|image| {
                image.virtual_placement
                    && image.virtual_screen == Some(self.active)
                    && matches(image.record.as_ref())
            })
            .find_map(|image| {
                Some(RelativeParent {
                    record: Arc::clone(&image.record),
                    anchor: {
                        let anchor = placeholder_anchor(self.active_grid(), image)?;
                        (i64::from(anchor.0), i64::from(anchor.1))
                    },
                })
            })
    }
}

fn cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    match record.protocol {
        GraphicsProtocol::Kitty => kitty_cell_dimensions(record),
        GraphicsProtocol::Sixel | GraphicsProtocol::Iterm2 => explicit_cell_dimensions(record),
    }
}

fn kitty_cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    if kitty_has_unsupported_dimension(record) {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: record.display.width,
            height: record.display.height,
        });
    }

    let (_, _, source_width, source_height) = record.source_rect()?;

    let columns = record
        .display
        .cell_columns
        .or_else(|| cell_dimension(record.display.width));
    let rows = record
        .display
        .cell_rows
        .or_else(|| cell_dimension(record.display.height));
    match (columns, rows) {
        (Some(columns), Some(rows)) => Ok((columns, rows)),
        (Some(columns), None) => Ok((
            columns,
            scaled_dimension(columns, source_width, source_height, false)?,
        )),
        (None, Some(rows)) => Ok((
            scaled_dimension(rows, source_width, source_height, true)?,
            rows,
        )),
        (None, None) => Err(ImagePlacementError::MissingCellDimensions {
            width: record.display.width,
            height: record.display.height,
        }),
    }
}

fn explicit_cell_dimensions(record: &ImageRecord) -> Result<(u32, u32), ImagePlacementError> {
    if record.display.width.is_some_and(is_non_cell_dimension)
        || record.display.height.is_some_and(is_non_cell_dimension)
    {
        return Err(ImagePlacementError::UnsupportedCellDimensions {
            width: record.display.width,
            height: record.display.height,
        });
    }

    match (
        cell_dimension(record.display.width),
        cell_dimension(record.display.height),
    ) {
        (Some(columns), Some(rows)) => Ok((columns, rows)),
        _ => Err(ImagePlacementError::MissingCellDimensions {
            width: record.display.width,
            height: record.display.height,
        }),
    }
}

fn cell_dimension(dimension: Option<ImageDimension>) -> Option<u32> {
    match dimension {
        Some(ImageDimension::Cells(value)) => Some(value),
        Some(ImageDimension::Auto) | None => None,
        Some(ImageDimension::Pixels(_) | ImageDimension::Percent(_)) => None,
    }
}

fn kitty_has_unsupported_dimension(record: &ImageRecord) -> bool {
    [record.display.width, record.display.height]
        .into_iter()
        .flatten()
        .any(|dimension| matches!(dimension, ImageDimension::Percent(_)))
}

fn is_non_cell_dimension(dimension: ImageDimension) -> bool {
    matches!(
        dimension,
        ImageDimension::Pixels(_) | ImageDimension::Percent(_)
    )
}

fn scaled_dimension(
    fixed: u32,
    source_width: u32,
    source_height: u32,
    width_from_height: bool,
) -> Result<u32, ImagePlacementError> {
    if fixed == 0 || source_width == 0 || source_height == 0 {
        return Err(ImagePlacementError::ZeroSize {
            columns: if width_from_height { 0 } else { fixed },
            rows: if width_from_height { fixed } else { 0 },
        });
    }
    let (numerator, denominator) = if width_from_height {
        (
            u64::from(fixed) * u64::from(source_width),
            u64::from(source_height),
        )
    } else {
        (
            u64::from(fixed) * u64::from(source_height),
            u64::from(source_width),
        )
    };
    let scaled = numerator
        .checked_add(denominator - 1)
        .map(|value| value / denominator)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ImagePlacementError::DimensionsTooLarge {
            columns: if width_from_height { 0 } else { fixed },
            rows: if width_from_height { fixed } else { 0 },
        })?;
    Ok(scaled.max(1))
}
