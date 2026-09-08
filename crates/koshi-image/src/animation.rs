//! Bounded animated-media decoding and validated RGBA frame values.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use image::{AnimationDecoder, ImageDecoder};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::codec::{
    decode_static_raster, guess_image_format, map_image_error, png_is_animated, raster_limits,
    webp_is_animated,
};
use crate::{
    checked_rgba_len, validate_dimensions, DecodedImage, GraphicsError, GraphicsProtocol,
    MAX_ANIMATION_FRAMES, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE,
};

/// The error returned when an animation value cannot satisfy its invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AnimationError {
    /// A frame delay has a zero denominator.
    #[error("animation delay denominator must be nonzero")]
    InvalidDelayDenominator,
    /// A finite animation has no playbacks.
    #[error("finite animation playback count must be nonzero")]
    InvalidPlaybackCount,
    /// An animation contains no frames.
    #[error("animation must contain at least one frame")]
    EmptyFrames,
    /// An animation contains more frames than the retained-frame limit.
    #[error("animation frame count exceeds the graphics limit")]
    TooManyFrames,
    /// A frame does not contain valid RGBA pixels.
    #[error("animation frame pixels are invalid")]
    InvalidFrameImage,
    /// Frames do not share one complete canvas size.
    #[error("animation frames do not share one canvas size")]
    InconsistentCanvas,
    /// Retained frame pixels exceed the shared image-byte limit.
    #[error("animation frame pixels exceed the graphics limit")]
    RetainedBytesTooLarge,
}

/// A nonnegative frame delay represented as a ratio with a millisecond numerator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FrameDelay {
    numerator_ms: u32,
    denominator_ms: u32,
}

impl FrameDelay {
    /// Construct a delay whose denominator is nonzero.
    pub fn new(numerator_ms: u32, denominator_ms: u32) -> Result<Self, AnimationError> {
        if denominator_ms == 0 {
            return Err(AnimationError::InvalidDelayDenominator);
        }
        Ok(Self {
            numerator_ms,
            denominator_ms,
        })
    }

    /// Return the numerator in milliseconds.
    #[must_use]
    pub fn numerator_ms(self) -> u32 {
        self.numerator_ms
    }

    /// Return the nonzero dimensionless divisor for the millisecond numerator.
    #[must_use]
    pub fn denominator_ms(self) -> u32 {
        self.denominator_ms
    }
}

impl<'de> Deserialize<'de> for FrameDelay {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameDelayFields {
            numerator_ms: u32,
            denominator_ms: u32,
        }

        let fields = FrameDelayFields::deserialize(deserializer)?;
        Self::new(fields.numerator_ms, fields.denominator_ms).map_err(de::Error::custom)
    }
}

/// The number of complete playbacks retained by an animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LoopPolicy {
    /// Play the frames a positive number of times.
    Finite(u32),
    /// Repeat the frames without a finite end.
    Infinite,
}

impl LoopPolicy {
    /// Construct a finite playback policy with a positive count.
    pub fn finite(total_playbacks: u32) -> Result<Self, AnimationError> {
        if total_playbacks == 0 {
            return Err(AnimationError::InvalidPlaybackCount);
        }
        Ok(Self::Finite(total_playbacks))
    }

    /// Return the finite playback count, or an absent value for infinite playback.
    #[must_use]
    pub fn total_playbacks(self) -> Option<u32> {
        match self {
            Self::Finite(total_playbacks) => Some(total_playbacks),
            Self::Infinite => None,
        }
    }

    /// Return whether this policy repeats without a finite end.
    #[must_use]
    pub fn is_infinite(self) -> bool {
        matches!(self, Self::Infinite)
    }
}

impl<'de> Deserialize<'de> for LoopPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum LoopPolicyFields {
            Finite(u32),
            Infinite,
        }

        match LoopPolicyFields::deserialize(deserializer)? {
            LoopPolicyFields::Finite(total_playbacks) => {
                Self::finite(total_playbacks).map_err(de::Error::custom)
            }
            LoopPolicyFields::Infinite => Ok(Self::Infinite),
        }
    }
}

/// One shared complete RGBA canvas and its display delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnimationFrame {
    image: Arc<DecodedImage>,
    delay: FrameDelay,
    gapless: bool,
}

impl AnimationFrame {
    /// Construct a frame after validating its RGBA dimensions and delay.
    pub fn new<I>(image: I, delay: FrameDelay) -> Result<Self, AnimationError>
    where
        I: Into<Arc<DecodedImage>>,
    {
        let image = image.into();
        validate_animation_image(&image)?;
        Ok(Self {
            image,
            delay,
            gapless: false,
        })
    }

    /// Construct a frame that is skipped without a display interval.
    pub fn new_gapless<I>(image: I) -> Result<Self, AnimationError>
    where
        I: Into<Arc<DecodedImage>>,
    {
        let image = image.into();
        validate_animation_image(&image)?;
        Ok(Self {
            image,
            delay: FrameDelay::new(0, 1)?,
            gapless: true,
        })
    }

    /// Return the complete RGBA canvas for this frame.
    #[must_use]
    pub fn image(&self) -> &DecodedImage {
        &self.image
    }

    /// Return a shared handle to the complete RGBA canvas for this frame.
    #[must_use]
    pub fn image_shared(&self) -> Arc<DecodedImage> {
        Arc::clone(&self.image)
    }

    /// Return the frame delay.
    #[must_use]
    pub fn delay(&self) -> FrameDelay {
        self.delay
    }

    /// Return whether this frame is skipped without a display interval.
    #[must_use]
    pub fn gapless(&self) -> bool {
        self.gapless
    }
}

impl<'de> Deserialize<'de> for AnimationFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct AnimationFrameFields {
            image: Arc<DecodedImage>,
            delay: FrameDelay,
            #[serde(default)]
            gapless: bool,
        }

        let fields = AnimationFrameFields::deserialize(deserializer)?;
        let mut frame = Self::new(fields.image, fields.delay).map_err(de::Error::custom)?;
        frame.gapless = fields.gapless;
        Ok(frame)
    }
}

/// A validated sequence of complete RGBA canvases and its loop policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAnimation {
    frames: Vec<AnimationFrame>,
    loop_policy: LoopPolicy,
}

impl DecodedAnimation {
    /// Construct an animation after validating frame count, canvases, and bytes.
    pub fn new(
        frames: Vec<AnimationFrame>,
        loop_policy: LoopPolicy,
    ) -> Result<Self, AnimationError> {
        if frames.is_empty() {
            return Err(AnimationError::EmptyFrames);
        }
        if frames.len() > MAX_ANIMATION_FRAMES {
            return Err(AnimationError::TooManyFrames);
        }
        if matches!(loop_policy, LoopPolicy::Finite(0)) {
            return Err(AnimationError::InvalidPlaybackCount);
        }

        let canvas = (frames[0].image.width, frames[0].image.height);
        let mut retained_bytes = 0usize;
        for frame in &frames {
            validate_animation_image(&frame.image)?;
            if (frame.image.width, frame.image.height) != canvas {
                return Err(AnimationError::InconsistentCanvas);
            }
            retained_bytes = retained_bytes
                .checked_add(frame.image.rgba.len())
                .ok_or(AnimationError::RetainedBytesTooLarge)?;
            if retained_bytes > MAX_IMAGE_BYTES {
                return Err(AnimationError::RetainedBytesTooLarge);
            }
        }

        Ok(Self {
            frames,
            loop_policy,
        })
    }

    /// Return the retained complete canvases in display order.
    #[must_use]
    pub fn frames(&self) -> &[AnimationFrame] {
        &self.frames
    }

    /// Return the number of retained frames.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Return the animation loop policy.
    #[must_use]
    pub fn loop_policy(&self) -> LoopPolicy {
        self.loop_policy
    }

    /// Return the shared canvas dimensions in pixels.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.frames[0].image.width, self.frames[0].image.height)
    }
}

impl Serialize for DecodedAnimation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("DecodedAnimation", 2)?;
        state.serialize_field("frames", &self.frames)?;
        state.serialize_field("loop_policy", &self.loop_policy)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for DecodedAnimation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DecodedAnimationFields {
            frames: BoundedAnimationFrames,
            loop_policy: LoopPolicy,
        }

        let fields = DecodedAnimationFields::deserialize(deserializer)?;
        Self::new(fields.frames.0, fields.loop_policy).map_err(de::Error::custom)
    }
}

struct BoundedAnimationFrames(Vec<AnimationFrame>);

impl<'de> Deserialize<'de> for BoundedAnimationFrames {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FramesVisitor;

        impl<'de> Visitor<'de> for FramesVisitor {
            type Value = BoundedAnimationFrames;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded sequence of animation frames")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let hint = sequence.size_hint().unwrap_or(0);
                if hint > MAX_ANIMATION_FRAMES {
                    return Err(de::Error::custom(
                        "animation frame count exceeds the graphics limit",
                    ));
                }

                let mut frames = Vec::new();
                if hint > 0 {
                    frames
                        .try_reserve(hint)
                        .map_err(|_| de::Error::custom("animation frame allocation failed"))?;
                }
                let mut retained_bytes = 0usize;
                while let Some(frame) = sequence.next_element_seed(BoundedAnimationFrameSeed {
                    remaining_bytes: MAX_IMAGE_BYTES.checked_sub(retained_bytes).ok_or_else(
                        || de::Error::custom("animation frame bytes exceed the graphics limit"),
                    )?,
                })? {
                    if frames.len() >= MAX_ANIMATION_FRAMES {
                        return Err(de::Error::custom(
                            "animation frame count exceeds the graphics limit",
                        ));
                    }
                    retained_bytes = retained_bytes
                        .checked_add(frame.image.rgba.len())
                        .ok_or_else(|| {
                            de::Error::custom("animation frame bytes exceed the graphics limit")
                        })?;
                    if retained_bytes > MAX_IMAGE_BYTES {
                        return Err(de::Error::custom(
                            "animation frame bytes exceed the graphics limit",
                        ));
                    }
                    frames
                        .try_reserve(1)
                        .map_err(|_| de::Error::custom("animation frame allocation failed"))?;
                    frames.push(frame);
                }
                Ok(BoundedAnimationFrames(frames))
            }
        }

        deserializer.deserialize_seq(FramesVisitor)
    }
}

struct BoundedAnimationFrameSeed {
    remaining_bytes: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedAnimationFrameSeed {
    type Value = AnimationFrame;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BoundedAnimationFrameVisitor {
            remaining_bytes: self.remaining_bytes,
        })
    }
}

struct BoundedAnimationFrameVisitor {
    remaining_bytes: usize,
}

impl<'de> Visitor<'de> for BoundedAnimationFrameVisitor {
    type Value = AnimationFrame;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an animation frame")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut image = None;
        let mut delay = None;
        let mut gapless = false;
        while let Some(field) = map.next_key::<AnimationFrameField>()? {
            match field {
                AnimationFrameField::Image => {
                    if image.is_some() {
                        return Err(de::Error::duplicate_field("image"));
                    }
                    image = Some(map.next_value_seed(BoundedDecodedImageSeed {
                        max_bytes: self.remaining_bytes,
                    })?);
                }
                AnimationFrameField::Delay => {
                    if delay.is_some() {
                        return Err(de::Error::duplicate_field("delay"));
                    }
                    delay = Some(map.next_value::<FrameDelay>()?);
                }
                AnimationFrameField::Gapless => {
                    gapless = map.next_value()?;
                }
                AnimationFrameField::Other => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let image = image.ok_or_else(|| de::Error::missing_field("image"))?;
        let delay = delay.ok_or_else(|| de::Error::missing_field("delay"))?;
        let mut frame = AnimationFrame::new(image, delay).map_err(de::Error::custom)?;
        frame.gapless = gapless;
        Ok(frame)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum AnimationFrameField {
    Image,
    Delay,
    Gapless,
    #[serde(other)]
    Other,
}

struct BoundedDecodedImageSeed {
    max_bytes: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedDecodedImageSeed {
    type Value = Arc<DecodedImage>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BoundedDecodedImageVisitor {
            max_bytes: self.max_bytes,
        })
    }
}

struct BoundedDecodedImageVisitor {
    max_bytes: usize,
}

impl<'de> Visitor<'de> for BoundedDecodedImageVisitor {
    type Value = Arc<DecodedImage>;

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
        while let Some(field) = map.next_key::<DecodedImageField>()? {
            match field {
                DecodedImageField::Width => {
                    if width.is_some() {
                        return Err(de::Error::duplicate_field("width"));
                    }
                    width = Some(map.next_value::<u32>()?);
                }
                DecodedImageField::Height => {
                    if height.is_some() {
                        return Err(de::Error::duplicate_field("height"));
                    }
                    height = Some(map.next_value::<u32>()?);
                }
                DecodedImageField::Rgba => {
                    if rgba.is_some() {
                        return Err(de::Error::duplicate_field("rgba"));
                    }
                    let limit = match (width, height) {
                        (Some(width), Some(height)) => {
                            let expected = expected_animation_bytes(width, height)
                                .map_err(de::Error::custom)?;
                            if expected > self.max_bytes {
                                return Err(de::Error::custom(
                                    "animation frame bytes exceed the graphics limit",
                                ));
                            }
                            expected
                        }
                        _ => self.max_bytes,
                    };
                    rgba = Some(map.next_value_seed(crate::BoundedBytesSeed::new(
                        limit,
                        "decoded image RGBA data",
                    ))?);
                }
                DecodedImageField::Other => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let width = width.ok_or_else(|| de::Error::missing_field("width"))?;
        let height = height.ok_or_else(|| de::Error::missing_field("height"))?;
        let rgba = rgba.ok_or_else(|| de::Error::missing_field("rgba"))?;
        let image = Arc::new(DecodedImage {
            width,
            height,
            rgba,
        });
        validate_animation_image(&image).map_err(de::Error::custom)?;
        Ok(image)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum DecodedImageField {
    Width,
    Height,
    Rgba,
    #[serde(other)]
    Other,
}

fn expected_animation_bytes(width: u32, height: u32) -> Result<usize, AnimationError> {
    let width = usize::try_from(width).map_err(|_| AnimationError::InvalidFrameImage)?;
    let height = usize::try_from(height).map_err(|_| AnimationError::InvalidFrameImage)?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_SIDE
        || height > MAX_IMAGE_SIDE
        || width
            .checked_mul(height)
            .is_none_or(|pixels| pixels > MAX_IMAGE_PIXELS)
    {
        return Err(AnimationError::InvalidFrameImage);
    }
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(AnimationError::InvalidFrameImage)?;
    if expected > MAX_IMAGE_BYTES {
        return Err(AnimationError::InvalidFrameImage);
    }
    Ok(expected)
}

/// A decoded static image or a bounded animation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecodedMedia {
    /// One validated static image.
    Static(DecodedImage),
    /// A validated animation with complete RGBA canvases.
    Animation(DecodedAnimation),
}

/// Decode a supported static or animated raster into bounded RGBA data.
pub fn decode_media(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    catch_unwind(AssertUnwindSafe(|| decode_media_inner(protocol, data)))
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn decode_media_inner(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let format = guess_image_format(protocol, data)?;
    match format {
        image::ImageFormat::Gif => {
            let scan = scan_gif(protocol, data)?;
            if scan.animated {
                decode_gif_animation(protocol, data, scan.loop_policy)
            } else {
                decode_static_raster(protocol, data).map(DecodedMedia::Static)
            }
        }
        image::ImageFormat::Png => {
            if png_is_animated(protocol, data)? {
                decode_apng_animation(protocol, data)
            } else {
                decode_static_raster(protocol, data).map(DecodedMedia::Static)
            }
        }
        image::ImageFormat::WebP => {
            if webp_is_animated(protocol, data)? {
                decode_webp_animation(protocol, data)
            } else {
                decode_static_raster(protocol, data).map(DecodedMedia::Static)
            }
        }
        _ => decode_static_raster(protocol, data).map(DecodedMedia::Static),
    }
}

fn decode_gif_animation(
    protocol: GraphicsProtocol,
    data: &[u8],
    loop_policy: LoopPolicy,
) -> Result<DecodedMedia, GraphicsError> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(data))
        .map_err(|error| map_image_error(protocol, error))?;
    decoder
        .set_limits(raster_limits())
        .map_err(|error| map_image_error(protocol, error))?;
    let dimensions = decoder.dimensions();
    collect_animation(protocol, decoder.into_frames(), dimensions, loop_policy)
}

fn decode_apng_animation(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(data), raster_limits())
        .map_err(|error| map_image_error(protocol, error))?;
    let dimensions = decoder.dimensions();
    let decoder = decoder
        .apng()
        .map_err(|error| map_image_error(protocol, error))?;
    let loop_policy = loop_policy_from_image(decoder.loop_count(), protocol)?;
    collect_animation(protocol, decoder.into_frames(), dimensions, loop_policy)
}

fn decode_webp_animation(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(data))
        .map_err(|error| map_image_error(protocol, error))?;
    decoder
        .set_limits(raster_limits())
        .map_err(|error| map_image_error(protocol, error))?;
    let dimensions = decoder.dimensions();
    let loop_policy = loop_policy_from_image(decoder.loop_count(), protocol)?;
    collect_animation(protocol, decoder.into_frames(), dimensions, loop_policy)
}

fn loop_policy_from_image(
    loop_count: image::metadata::LoopCount,
    protocol: GraphicsProtocol,
) -> Result<LoopPolicy, GraphicsError> {
    match loop_count {
        image::metadata::LoopCount::Infinite => Ok(LoopPolicy::Infinite),
        image::metadata::LoopCount::Finite(total_playbacks) => {
            LoopPolicy::finite(total_playbacks.get())
                .map_err(|_| GraphicsError::DecodeFailure { protocol })
        }
    }
}

fn collect_animation<'a>(
    protocol: GraphicsProtocol,
    frames: image::Frames<'a>,
    dimensions: (u32, u32),
    loop_policy: LoopPolicy,
) -> Result<DecodedMedia, GraphicsError> {
    let mut retained = Vec::new();
    let mut retained_bytes = 0usize;
    for frame_result in frames {
        if retained.len() >= MAX_ANIMATION_FRAMES {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let frame = frame_result.map_err(|error| map_image_error(protocol, error))?;
        if frame.left() != 0 || frame.top() != 0 {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let buffer = frame.buffer();
        let width = buffer.width();
        let height = buffer.height();
        if (width, height) != dimensions {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let width_usize =
            usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
        let height_usize =
            usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
        validate_dimensions(protocol, width_usize, height_usize)?;
        let expected = checked_rgba_len(protocol, width_usize, height_usize)?;
        if frame.buffer().as_raw().len() != expected {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let remaining_bytes = MAX_IMAGE_BYTES
            .checked_sub(retained_bytes)
            .ok_or(GraphicsError::ImageTooLarge { protocol })?;
        if expected > remaining_bytes {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let delay = frame.delay().numer_denom_ms();
        let rgba = frame.into_buffer().into_raw();
        if rgba.len() != expected {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        retained_bytes = retained_bytes
            .checked_add(rgba.len())
            .ok_or(GraphicsError::ImageTooLarge { protocol })?;
        if retained_bytes > MAX_IMAGE_BYTES {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let image = DecodedImage {
            width,
            height,
            rgba,
        };
        let delay = FrameDelay::new(delay.0, delay.1)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let animation_frame = AnimationFrame::new(image, delay)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        retained
            .try_reserve(1)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        retained.push(animation_frame);
    }

    if retained.is_empty() {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    DecodedAnimation::new(retained, loop_policy)
        .map(DecodedMedia::Animation)
        .map_err(|error| map_animation_error(protocol, error))
}

fn map_animation_error(protocol: GraphicsProtocol, error: AnimationError) -> GraphicsError {
    match error {
        AnimationError::TooManyFrames | AnimationError::RetainedBytesTooLarge => {
            GraphicsError::ImageTooLarge { protocol }
        }
        AnimationError::InvalidDelayDenominator
        | AnimationError::InvalidPlaybackCount
        | AnimationError::EmptyFrames
        | AnimationError::InvalidFrameImage
        | AnimationError::InconsistentCanvas => GraphicsError::DecodeFailure { protocol },
    }
}

fn validate_animation_image(image: &DecodedImage) -> Result<(), AnimationError> {
    let expected = expected_animation_bytes(image.width, image.height)?;
    if image.rgba.len() != expected {
        return Err(AnimationError::InvalidFrameImage);
    }
    Ok(())
}

struct GifScan {
    animated: bool,
    loop_policy: LoopPolicy,
}

#[derive(Clone, Copy)]
enum GifScanError {
    Malformed,
    InvalidMetadata,
    TooManyFrames,
}

fn scan_gif(protocol: GraphicsProtocol, data: &[u8]) -> Result<GifScan, GraphicsError> {
    scan_gif_inner(data).map_err(|error| match error {
        GifScanError::Malformed => GraphicsError::DecodeFailure { protocol },
        GifScanError::InvalidMetadata => GraphicsError::DecodeFailure { protocol },
        GifScanError::TooManyFrames => GraphicsError::ImageTooLarge { protocol },
    })
}

fn scan_gif_inner(data: &[u8]) -> Result<GifScan, GifScanError> {
    let signature = data.get(0..6).ok_or(GifScanError::Malformed)?;
    if signature != b"GIF87a" && signature != b"GIF89a" {
        return Err(GifScanError::Malformed);
    }
    let logical_screen = data.get(6..13).ok_or(GifScanError::Malformed)?;
    let packed = *logical_screen.get(4).ok_or(GifScanError::Malformed)?;
    let mut offset = 13usize;
    if packed & 0x80 != 0 {
        let table_entries = 1usize
            .checked_shl(u32::from((packed & 0x07) + 1))
            .ok_or(GifScanError::Malformed)?;
        let table_bytes = table_entries
            .checked_mul(3)
            .ok_or(GifScanError::Malformed)?;
        take_gif(data, &mut offset, table_bytes)?;
    }

    let mut image_count = 0usize;
    let mut loop_policy = LoopPolicy::finite(1).map_err(|_| GifScanError::Malformed)?;
    let mut has_loop_extension = false;
    loop {
        let introducer = *take_gif(data, &mut offset, 1)?
            .first()
            .ok_or(GifScanError::Malformed)?;
        match introducer {
            0x3b => {
                return Ok(GifScan {
                    animated: image_count > 1 || has_loop_extension,
                    loop_policy,
                });
            }
            0x21 => {
                let label = *take_gif(data, &mut offset, 1)?
                    .first()
                    .ok_or(GifScanError::Malformed)?;
                match label {
                    0xff => {
                        if let Some(policy) = scan_gif_application_extension(data, &mut offset)? {
                            loop_policy = policy;
                            has_loop_extension = true;
                        }
                    }
                    0x01 => {
                        let size = *take_gif(data, &mut offset, 1)?
                            .first()
                            .ok_or(GifScanError::Malformed)?;
                        if size != 12 {
                            return Err(GifScanError::Malformed);
                        }
                        take_gif(data, &mut offset, usize::from(size))?;
                        skip_gif_subblocks(data, &mut offset)?;
                    }
                    0xf9 => {
                        let size = *take_gif(data, &mut offset, 1)?
                            .first()
                            .ok_or(GifScanError::Malformed)?;
                        if size != 4 {
                            return Err(GifScanError::Malformed);
                        }
                        take_gif(data, &mut offset, usize::from(size))?;
                        let terminator = *take_gif(data, &mut offset, 1)?
                            .first()
                            .ok_or(GifScanError::Malformed)?;
                        if terminator != 0 {
                            return Err(GifScanError::Malformed);
                        }
                    }
                    _ => skip_gif_subblocks(data, &mut offset)?,
                }
            }
            0x2c => {
                let descriptor = take_gif(data, &mut offset, 9)?;
                let image_packed = *descriptor.get(8).ok_or(GifScanError::Malformed)?;
                if image_packed & 0x80 != 0 {
                    let table_entries = 1usize
                        .checked_shl(u32::from((image_packed & 0x07) + 1))
                        .ok_or(GifScanError::Malformed)?;
                    let table_bytes = table_entries
                        .checked_mul(3)
                        .ok_or(GifScanError::Malformed)?;
                    take_gif(data, &mut offset, table_bytes)?;
                }
                take_gif(data, &mut offset, 1)?;
                skip_gif_subblocks(data, &mut offset)?;
                image_count = image_count
                    .checked_add(1)
                    .ok_or(GifScanError::TooManyFrames)?;
                if image_count > MAX_ANIMATION_FRAMES {
                    return Err(GifScanError::TooManyFrames);
                }
            }
            _ => return Err(GifScanError::Malformed),
        }
    }
}

fn scan_gif_application_extension(
    data: &[u8],
    offset: &mut usize,
) -> Result<Option<LoopPolicy>, GifScanError> {
    let identifier_size = *take_gif(data, offset, 1)?
        .first()
        .ok_or(GifScanError::Malformed)?;
    let identifier = take_gif(data, offset, usize::from(identifier_size))?;
    let recognized = identifier == b"NETSCAPE2.0" || identifier == b"ANIMEXTS1.0";

    let mut first_data = None;
    loop {
        let block_size = match take_gif(data, offset, 1) {
            Ok(bytes) => *bytes.first().ok_or(GifScanError::Malformed)?,
            Err(_error) if recognized => return Err(GifScanError::InvalidMetadata),
            Err(error) => return Err(error),
        };
        if block_size == 0 {
            break;
        }
        let block = match take_gif(data, offset, usize::from(block_size)) {
            Ok(block) => block,
            Err(_error) if recognized => return Err(GifScanError::InvalidMetadata),
            Err(error) => return Err(error),
        };
        if recognized && first_data.is_none() {
            first_data = Some(block);
        }
    }

    if !recognized {
        return Ok(None);
    }
    let block = first_data.ok_or(GifScanError::InvalidMetadata)?;
    if block.len() < 3 || block[0] != 1 {
        return Err(GifScanError::InvalidMetadata);
    }
    let repeat_count = u16::from_le_bytes([
        *block.get(1).ok_or(GifScanError::InvalidMetadata)?,
        *block.get(2).ok_or(GifScanError::InvalidMetadata)?,
    ]);
    if repeat_count == 0 {
        return Ok(Some(LoopPolicy::Infinite));
    }
    let total_playbacks = u32::from(repeat_count)
        .checked_add(1)
        .ok_or(GifScanError::InvalidMetadata)?;
    let policy = LoopPolicy::finite(total_playbacks).map_err(|_| GifScanError::InvalidMetadata)?;
    Ok(Some(policy))
}

fn take_gif<'a>(
    data: &'a [u8],
    offset: &mut usize,
    length: usize,
) -> Result<&'a [u8], GifScanError> {
    let end = offset.checked_add(length).ok_or(GifScanError::Malformed)?;
    let bytes = data.get(*offset..end).ok_or(GifScanError::Malformed)?;
    *offset = end;
    Ok(bytes)
}

fn skip_gif_subblocks(data: &[u8], offset: &mut usize) -> Result<(), GifScanError> {
    loop {
        let block_size = *take_gif(data, offset, 1)?
            .first()
            .ok_or(GifScanError::Malformed)?;
        if block_size == 0 {
            return Ok(());
        }
        take_gif(data, offset, usize::from(block_size))?;
    }
}

#[cfg(test)]
mod tests;
