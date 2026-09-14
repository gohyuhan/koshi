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
    build_raster_limits, decode_static_raster, guess_image_format, map_image_error,
    png_is_animated, webp_is_animated,
};
use crate::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsError,
    GraphicsProtocol, MAX_ANIMATION_FRAME_COUNT, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
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
    /// Return a delay, or [`AnimationError::InvalidDelayDenominator`] when `denominator_ms == 0`.
    pub fn from_millisecond_ratio(
        numerator_ms: u32,
        denominator_ms: u32,
    ) -> Result<Self, AnimationError> {
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
    pub fn get_numerator_ms(self) -> u32 {
        self.numerator_ms
    }

    /// Return the nonzero denominator of the millisecond ratio.
    #[must_use]
    pub fn get_denominator_ms(self) -> u32 {
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

        let frame_delay_fields = FrameDelayFields::deserialize(deserializer)?;
        Self::from_millisecond_ratio(
            frame_delay_fields.numerator_ms,
            frame_delay_fields.denominator_ms,
        )
        .map_err(de::Error::custom)
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
    /// Return a finite policy, or [`AnimationError::InvalidPlaybackCount`] when `total_playbacks == 0`.
    pub fn from_finite_playback_count(total_playbacks: u32) -> Result<Self, AnimationError> {
        if total_playbacks == 0 {
            return Err(AnimationError::InvalidPlaybackCount);
        }
        Ok(Self::Finite(total_playbacks))
    }

    /// Return the finite playback count, or an absent value for infinite playback.
    #[must_use]
    pub fn get_total_playback_count(self) -> Option<u32> {
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
                Self::from_finite_playback_count(total_playbacks).map_err(de::Error::custom)
            }
            LoopPolicyFields::Infinite => Ok(Self::Infinite),
        }
    }
}

/// One shared complete RGBA canvas and its display delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnimationFrame {
    #[serde(rename = "image")]
    decoded_image: Arc<DecodedImage>,
    #[serde(rename = "delay")]
    frame_delay: FrameDelay,
    #[serde(rename = "gapless")]
    is_gapless: bool,
}

impl AnimationFrame {
    /// Return a frame, or [`AnimationError::InvalidFrameImage`] for invalid RGBA dimensions.
    pub fn from_image_and_delay<ImageSource>(
        decoded_image: ImageSource,
        frame_delay: FrameDelay,
    ) -> Result<Self, AnimationError>
    where
        ImageSource: Into<Arc<DecodedImage>>,
    {
        let decoded_image = decoded_image.into();
        validate_animation_image(&decoded_image)?;
        Ok(Self {
            decoded_image,
            frame_delay,
            is_gapless: false,
        })
    }

    /// Return a zero-delay gapless frame, or [`AnimationError::InvalidFrameImage`] for invalid RGBA dimensions.
    pub fn from_gapless_image<ImageSource>(
        decoded_image: ImageSource,
    ) -> Result<Self, AnimationError>
    where
        ImageSource: Into<Arc<DecodedImage>>,
    {
        let decoded_image = decoded_image.into();
        validate_animation_image(&decoded_image)?;
        Ok(Self {
            decoded_image,
            frame_delay: FrameDelay::from_millisecond_ratio(0, 1)?,
            is_gapless: true,
        })
    }

    /// Return the complete RGBA canvas for this frame.
    #[must_use]
    pub fn get_decoded_image(&self) -> &DecodedImage {
        &self.decoded_image
    }

    /// Return a shared handle to the complete RGBA canvas for this frame.
    #[must_use]
    pub fn clone_decoded_image(&self) -> Arc<DecodedImage> {
        Arc::clone(&self.decoded_image)
    }

    /// Return the frame delay.
    #[must_use]
    pub fn get_frame_delay(&self) -> FrameDelay {
        self.frame_delay
    }

    /// Return whether this frame is skipped without a display interval.
    #[must_use]
    pub fn is_gapless(&self) -> bool {
        self.is_gapless
    }
}

impl<'de> Deserialize<'de> for AnimationFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct AnimationFrameFields {
            #[serde(rename = "image")]
            decoded_image: Arc<DecodedImage>,
            #[serde(rename = "delay")]
            frame_delay: FrameDelay,
            #[serde(default)]
            #[serde(rename = "gapless")]
            is_gapless: bool,
        }

        let animation_frame_fields = AnimationFrameFields::deserialize(deserializer)?;
        let mut animation_frame = Self::from_image_and_delay(
            animation_frame_fields.decoded_image,
            animation_frame_fields.frame_delay,
        )
        .map_err(de::Error::custom)?;
        animation_frame.is_gapless = animation_frame_fields.is_gapless;
        Ok(animation_frame)
    }
}

/// A validated sequence of complete RGBA canvases and its loop policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAnimation {
    frames: Vec<AnimationFrame>,
    loop_policy: LoopPolicy,
}

impl DecodedAnimation {
    /// Return an animation after validating frames, canvases, bytes, and playback policy.
    ///
    /// Returns [`AnimationError`] when the frames are empty, too numerous, inconsistent, invalid,
    /// or exceed the retained-byte limit, or when the policy has zero finite playbacks.
    pub fn from_frames_and_loop_policy(
        frames: Vec<AnimationFrame>,
        loop_policy: LoopPolicy,
    ) -> Result<Self, AnimationError> {
        if frames.is_empty() {
            return Err(AnimationError::EmptyFrames);
        }
        if frames.len() > MAX_ANIMATION_FRAME_COUNT {
            return Err(AnimationError::TooManyFrames);
        }
        if matches!(loop_policy, LoopPolicy::Finite(0)) {
            return Err(AnimationError::InvalidPlaybackCount);
        }

        let canvas_dimensions = (
            frames[0].decoded_image.pixel_width,
            frames[0].decoded_image.pixel_height,
        );
        let mut retained_byte_count = 0usize;
        for frame in &frames {
            validate_animation_image(&frame.decoded_image)?;
            if (
                frame.decoded_image.pixel_width,
                frame.decoded_image.pixel_height,
            ) != canvas_dimensions
            {
                return Err(AnimationError::InconsistentCanvas);
            }
            retained_byte_count = retained_byte_count
                .checked_add(frame.decoded_image.rgba_bytes.len())
                .ok_or(AnimationError::RetainedBytesTooLarge)?;
            if retained_byte_count > MAX_IMAGE_BYTE_COUNT {
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
    pub fn list_frames(&self) -> &[AnimationFrame] {
        &self.frames
    }

    /// Return the number of retained frames.
    #[must_use]
    pub fn get_frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Return the animation loop policy.
    #[must_use]
    pub fn get_loop_policy(&self) -> LoopPolicy {
        self.loop_policy
    }

    /// Return the shared canvas dimensions in pixels.
    #[must_use]
    pub fn get_image_pixel_dimensions(&self) -> (u32, u32) {
        (
            self.frames[0].decoded_image.pixel_width,
            self.frames[0].decoded_image.pixel_height,
        )
    }
}

impl Serialize for DecodedAnimation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut serialized_fields = serializer.serialize_struct("DecodedAnimation", 2)?;
        serialized_fields.serialize_field("frames", &self.frames)?;
        serialized_fields.serialize_field("loop_policy", &self.loop_policy)?;
        serialized_fields.end()
    }
}

impl<'de> Deserialize<'de> for DecodedAnimation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DecodedAnimationFields {
            #[serde(rename = "frames")]
            animation_frames: BoundedAnimationFrames,
            loop_policy: LoopPolicy,
        }

        let decoded_animation_fields = DecodedAnimationFields::deserialize(deserializer)?;
        Self::from_frames_and_loop_policy(
            decoded_animation_fields.animation_frames.animation_frames,
            decoded_animation_fields.loop_policy,
        )
        .map_err(de::Error::custom)
    }
}

struct BoundedAnimationFrames {
    animation_frames: Vec<AnimationFrame>,
}

impl<'de> Deserialize<'de> for BoundedAnimationFrames {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AnimationFramesVisitor;

        impl<'de> Visitor<'de> for AnimationFramesVisitor {
            type Value = BoundedAnimationFrames;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded sequence of animation frames")
            }

            fn visit_seq<A>(self, mut frame_sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let sequence_size_hint = frame_sequence.size_hint().unwrap_or(0);
                if sequence_size_hint > MAX_ANIMATION_FRAME_COUNT {
                    return Err(de::Error::custom(
                        "animation frame count exceeds the graphics limit",
                    ));
                }

                let mut animation_frames = Vec::new();
                if sequence_size_hint > 0 {
                    animation_frames
                        .try_reserve(sequence_size_hint)
                        .map_err(|_| de::Error::custom("animation frame allocation failed"))?;
                }
                let mut retained_byte_count = 0usize;
                while let Some(animation_frame) =
                    frame_sequence.next_element_seed(BoundedAnimationFrameSeed {
                        remaining_byte_count: MAX_IMAGE_BYTE_COUNT
                            .checked_sub(retained_byte_count)
                            .ok_or_else(|| {
                                de::Error::custom("animation frame bytes exceed the graphics limit")
                            })?,
                    })?
                {
                    if animation_frames.len() >= MAX_ANIMATION_FRAME_COUNT {
                        return Err(de::Error::custom(
                            "animation frame count exceeds the graphics limit",
                        ));
                    }
                    retained_byte_count = retained_byte_count
                        .checked_add(animation_frame.decoded_image.rgba_bytes.len())
                        .ok_or_else(|| {
                            de::Error::custom("animation frame bytes exceed the graphics limit")
                        })?;
                    if retained_byte_count > MAX_IMAGE_BYTE_COUNT {
                        return Err(de::Error::custom(
                            "animation frame bytes exceed the graphics limit",
                        ));
                    }
                    animation_frames
                        .try_reserve(1)
                        .map_err(|_| de::Error::custom("animation frame allocation failed"))?;
                    animation_frames.push(animation_frame);
                }
                Ok(BoundedAnimationFrames { animation_frames })
            }
        }

        deserializer.deserialize_seq(AnimationFramesVisitor)
    }
}

struct BoundedAnimationFrameSeed {
    remaining_byte_count: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedAnimationFrameSeed {
    type Value = AnimationFrame;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BoundedAnimationFrameVisitor {
            remaining_byte_count: self.remaining_byte_count,
        })
    }
}

struct BoundedAnimationFrameVisitor {
    remaining_byte_count: usize,
}

impl<'de> Visitor<'de> for BoundedAnimationFrameVisitor {
    type Value = AnimationFrame;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an animation frame")
    }

    fn visit_map<A>(self, mut map_access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut decoded_image = None;
        let mut frame_delay = None;
        let mut is_gapless = false;
        while let Some(field_name) = map_access.next_key::<AnimationFrameField>()? {
            match field_name {
                AnimationFrameField::Image => {
                    if decoded_image.is_some() {
                        return Err(de::Error::duplicate_field("image"));
                    }
                    decoded_image = Some(map_access.next_value_seed(BoundedDecodedImageSeed {
                        maximum_byte_count: self.remaining_byte_count,
                    })?);
                }
                AnimationFrameField::Delay => {
                    if frame_delay.is_some() {
                        return Err(de::Error::duplicate_field("delay"));
                    }
                    frame_delay = Some(map_access.next_value::<FrameDelay>()?);
                }
                AnimationFrameField::Gapless => {
                    is_gapless = map_access.next_value()?;
                }
                AnimationFrameField::Other => {
                    let _: de::IgnoredAny = map_access.next_value()?;
                }
            }
        }
        let decoded_image = decoded_image.ok_or_else(|| de::Error::missing_field("image"))?;
        let frame_delay = frame_delay.ok_or_else(|| de::Error::missing_field("delay"))?;
        let mut animation_frame = AnimationFrame::from_image_and_delay(decoded_image, frame_delay)
            .map_err(de::Error::custom)?;
        animation_frame.is_gapless = is_gapless;
        Ok(animation_frame)
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
    maximum_byte_count: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedDecodedImageSeed {
    type Value = Arc<DecodedImage>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BoundedDecodedImageVisitor {
            maximum_byte_count: self.maximum_byte_count,
        })
    }
}

struct BoundedDecodedImageVisitor {
    maximum_byte_count: usize,
}

impl<'de> Visitor<'de> for BoundedDecodedImageVisitor {
    type Value = Arc<DecodedImage>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded decoded RGBA image")
    }

    fn visit_map<A>(self, mut map_access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut pixel_width = None;
        let mut pixel_height = None;
        let mut rgba_bytes = None;
        while let Some(field_name) = map_access.next_key::<DecodedImageField>()? {
            match field_name {
                DecodedImageField::Width => {
                    if pixel_width.is_some() {
                        return Err(de::Error::duplicate_field("width"));
                    }
                    pixel_width = Some(map_access.next_value::<u32>()?);
                }
                DecodedImageField::Height => {
                    if pixel_height.is_some() {
                        return Err(de::Error::duplicate_field("height"));
                    }
                    pixel_height = Some(map_access.next_value::<u32>()?);
                }
                DecodedImageField::Rgba => {
                    if rgba_bytes.is_some() {
                        return Err(de::Error::duplicate_field("rgba"));
                    }
                    let rgba_byte_limit = match (pixel_width, pixel_height) {
                        (Some(pixel_width), Some(pixel_height)) => {
                            let expected_rgba_byte_count =
                                compute_expected_animation_byte_count(pixel_width, pixel_height)
                                    .map_err(de::Error::custom)?;
                            if expected_rgba_byte_count > self.maximum_byte_count {
                                return Err(de::Error::custom(
                                    "animation frame bytes exceed the graphics limit",
                                ));
                            }
                            expected_rgba_byte_count
                        }
                        _ => self.maximum_byte_count,
                    };
                    rgba_bytes = Some(map_access.next_value_seed(
                        crate::BoundedBytesSeed::from_byte_limit_and_error_label(
                            rgba_byte_limit,
                            "decoded image RGBA data",
                        ),
                    )?);
                }
                DecodedImageField::Other => {
                    let _: de::IgnoredAny = map_access.next_value()?;
                }
            }
        }
        let pixel_width = pixel_width.ok_or_else(|| de::Error::missing_field("width"))?;
        let pixel_height = pixel_height.ok_or_else(|| de::Error::missing_field("height"))?;
        let rgba_bytes = rgba_bytes.ok_or_else(|| de::Error::missing_field("rgba"))?;
        let decoded_image = Arc::new(DecodedImage {
            pixel_width,
            pixel_height,
            rgba_bytes,
        });
        validate_animation_image(&decoded_image).map_err(de::Error::custom)?;
        Ok(decoded_image)
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

fn compute_expected_animation_byte_count(
    pixel_width: u32,
    pixel_height: u32,
) -> Result<usize, AnimationError> {
    let pixel_width =
        usize::try_from(pixel_width).map_err(|_| AnimationError::InvalidFrameImage)?;
    let pixel_height =
        usize::try_from(pixel_height).map_err(|_| AnimationError::InvalidFrameImage)?;
    if pixel_width == 0
        || pixel_height == 0
        || pixel_width > MAX_IMAGE_SIDE_PIXEL_COUNT
        || pixel_height > MAX_IMAGE_SIDE_PIXEL_COUNT
        || pixel_width
            .checked_mul(pixel_height)
            .is_none_or(|pixel_count| pixel_count > MAX_IMAGE_PIXEL_COUNT)
    {
        return Err(AnimationError::InvalidFrameImage);
    }
    let expected_rgba_byte_count = pixel_width
        .checked_mul(pixel_height)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or(AnimationError::InvalidFrameImage)?;
    if expected_rgba_byte_count > MAX_IMAGE_BYTE_COUNT {
        return Err(AnimationError::InvalidFrameImage);
    }
    Ok(expected_rgba_byte_count)
}

/// A decoded static image or a bounded animation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecodedMedia {
    /// One validated static image.
    Static(DecodedImage),
    /// A validated animation with complete RGBA canvases.
    Animation(DecodedAnimation),
}

/// Decode supported static or animated raster data into bounded RGBA values.
///
/// Returns `Static` for BMP, GIF, JPEG, PNG, TIFF, and non-animated WebP bytes. Returns `Animation`
/// for animated GIF, PNG, or WebP bytes, and returns [`GraphicsError`] for unsupported, malformed,
/// or oversized data.
pub fn decode_media(
    protocol: GraphicsProtocol,
    encoded_media_bytes: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    if encoded_media_bytes.len() > MAX_IMAGE_BYTE_COUNT {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    catch_unwind(AssertUnwindSafe(|| {
        decode_media_inner(protocol, encoded_media_bytes)
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn decode_media_inner(
    protocol: GraphicsProtocol,
    encoded_media_bytes: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let image_format = guess_image_format(protocol, encoded_media_bytes)?;
    match image_format {
        image::ImageFormat::Gif => {
            let gif_scan = scan_gif(protocol, encoded_media_bytes)?;
            if gif_scan.is_animated {
                decode_gif_animation(protocol, encoded_media_bytes, gif_scan.loop_policy)
            } else {
                decode_static_raster(protocol, encoded_media_bytes).map(DecodedMedia::Static)
            }
        }
        image::ImageFormat::Png => {
            if png_is_animated(protocol, encoded_media_bytes)? {
                decode_apng_animation(protocol, encoded_media_bytes)
            } else {
                decode_static_raster(protocol, encoded_media_bytes).map(DecodedMedia::Static)
            }
        }
        image::ImageFormat::WebP => {
            if webp_is_animated(protocol, encoded_media_bytes)? {
                decode_webp_animation(protocol, encoded_media_bytes)
            } else {
                decode_static_raster(protocol, encoded_media_bytes).map(DecodedMedia::Static)
            }
        }
        _ => decode_static_raster(protocol, encoded_media_bytes).map(DecodedMedia::Static),
    }
}

fn decode_gif_animation(
    protocol: GraphicsProtocol,
    encoded_gif_bytes: &[u8],
    loop_policy: LoopPolicy,
) -> Result<DecodedMedia, GraphicsError> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(encoded_gif_bytes))
        .map_err(|image_error| map_image_error(protocol, image_error))?;
    decoder
        .set_limits(build_raster_limits())
        .map_err(|image_error| map_image_error(protocol, image_error))?;
    let canvas_pixel_dimensions = decoder.dimensions();
    collect_decoded_animation(
        protocol,
        decoder.into_frames(),
        canvas_pixel_dimensions,
        loop_policy,
    )
}

fn decode_apng_animation(
    protocol: GraphicsProtocol,
    encoded_png_bytes: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let decoder = image::codecs::png::PngDecoder::with_limits(
        Cursor::new(encoded_png_bytes),
        build_raster_limits(),
    )
    .map_err(|image_error| map_image_error(protocol, image_error))?;
    let canvas_pixel_dimensions = decoder.dimensions();
    let decoder = decoder
        .apng()
        .map_err(|image_error| map_image_error(protocol, image_error))?;
    let loop_policy = loop_policy_from_image(decoder.loop_count(), protocol)?;
    collect_decoded_animation(
        protocol,
        decoder.into_frames(),
        canvas_pixel_dimensions,
        loop_policy,
    )
}

fn decode_webp_animation(
    protocol: GraphicsProtocol,
    encoded_webp_bytes: &[u8],
) -> Result<DecodedMedia, GraphicsError> {
    let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(encoded_webp_bytes))
        .map_err(|image_error| map_image_error(protocol, image_error))?;
    decoder
        .set_limits(build_raster_limits())
        .map_err(|image_error| map_image_error(protocol, image_error))?;
    let canvas_pixel_dimensions = decoder.dimensions();
    let loop_policy = loop_policy_from_image(decoder.loop_count(), protocol)?;
    collect_decoded_animation(
        protocol,
        decoder.into_frames(),
        canvas_pixel_dimensions,
        loop_policy,
    )
}

fn loop_policy_from_image(
    loop_count: image::metadata::LoopCount,
    protocol: GraphicsProtocol,
) -> Result<LoopPolicy, GraphicsError> {
    match loop_count {
        image::metadata::LoopCount::Infinite => Ok(LoopPolicy::Infinite),
        image::metadata::LoopCount::Finite(total_playbacks) => {
            LoopPolicy::from_finite_playback_count(total_playbacks.get())
                .map_err(|_| GraphicsError::DecodeFailure { protocol })
        }
    }
}

fn collect_decoded_animation<'a>(
    protocol: GraphicsProtocol,
    decoded_frames: image::Frames<'a>,
    canvas_pixel_dimensions: (u32, u32),
    loop_policy: LoopPolicy,
) -> Result<DecodedMedia, GraphicsError> {
    let mut retained_animation_frames = Vec::new();
    let mut retained_byte_count = 0usize;
    for frame_result in decoded_frames {
        if retained_animation_frames.len() >= MAX_ANIMATION_FRAME_COUNT {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let decoded_frame =
            frame_result.map_err(|image_error| map_image_error(protocol, image_error))?;
        if decoded_frame.left() != 0 || decoded_frame.top() != 0 {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let decoded_frame_buffer = decoded_frame.buffer();
        let frame_width = decoded_frame_buffer.width();
        let frame_height = decoded_frame_buffer.height();
        if (frame_width, frame_height) != canvas_pixel_dimensions {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let frame_pixel_width =
            usize::try_from(frame_width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
        let frame_pixel_height =
            usize::try_from(frame_height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
        validate_image_dimensions(protocol, frame_pixel_width, frame_pixel_height)?;
        let expected_rgba_byte_count =
            compute_rgba_byte_count(protocol, frame_pixel_width, frame_pixel_height)?;
        if decoded_frame_buffer.as_raw().len() != expected_rgba_byte_count {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let remaining_image_byte_count = MAX_IMAGE_BYTE_COUNT
            .checked_sub(retained_byte_count)
            .ok_or(GraphicsError::ImageTooLarge { protocol })?;
        if expected_rgba_byte_count > remaining_image_byte_count {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let frame_delay_milliseconds = decoded_frame.delay().numer_denom_ms();
        let frame_rgba_bytes = decoded_frame.into_buffer().into_raw();
        if frame_rgba_bytes.len() != expected_rgba_byte_count {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        retained_byte_count = retained_byte_count
            .checked_add(frame_rgba_bytes.len())
            .ok_or(GraphicsError::ImageTooLarge { protocol })?;
        if retained_byte_count > MAX_IMAGE_BYTE_COUNT {
            return Err(GraphicsError::ImageTooLarge { protocol });
        }
        let decoded_image = DecodedImage {
            pixel_width: frame_width,
            pixel_height: frame_height,
            rgba_bytes: frame_rgba_bytes,
        };
        let frame_delay = FrameDelay::from_millisecond_ratio(
            frame_delay_milliseconds.0,
            frame_delay_milliseconds.1,
        )
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let animation_frame = AnimationFrame::from_image_and_delay(decoded_image, frame_delay)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        retained_animation_frames
            .try_reserve(1)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        retained_animation_frames.push(animation_frame);
    }

    if retained_animation_frames.is_empty() {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    DecodedAnimation::from_frames_and_loop_policy(retained_animation_frames, loop_policy)
        .map(DecodedMedia::Animation)
        .map_err(|animation_error| map_animation_error(protocol, animation_error))
}

fn map_animation_error(
    protocol: GraphicsProtocol,
    animation_error: AnimationError,
) -> GraphicsError {
    match animation_error {
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

fn validate_animation_image(decoded_image: &DecodedImage) -> Result<(), AnimationError> {
    let expected_rgba_byte_count = compute_expected_animation_byte_count(
        decoded_image.pixel_width,
        decoded_image.pixel_height,
    )?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err(AnimationError::InvalidFrameImage);
    }
    Ok(())
}

struct GifAnimationScan {
    is_animated: bool,
    loop_policy: LoopPolicy,
}

#[derive(Clone, Copy)]
enum GifAnimationScanError {
    Malformed,
    InvalidMetadata,
    TooManyFrames,
}

fn scan_gif(
    protocol: GraphicsProtocol,
    encoded_gif_bytes: &[u8],
) -> Result<GifAnimationScan, GraphicsError> {
    parse_gif_animation_metadata(encoded_gif_bytes).map_err(|gif_scan_error| match gif_scan_error {
        GifAnimationScanError::Malformed | GifAnimationScanError::InvalidMetadata => {
            GraphicsError::DecodeFailure { protocol }
        }
        GifAnimationScanError::TooManyFrames => GraphicsError::ImageTooLarge { protocol },
    })
}

fn parse_gif_animation_metadata(
    encoded_gif_bytes: &[u8],
) -> Result<GifAnimationScan, GifAnimationScanError> {
    let gif_signature = encoded_gif_bytes
        .get(0..6)
        .ok_or(GifAnimationScanError::Malformed)?;
    if gif_signature != b"GIF87a" && gif_signature != b"GIF89a" {
        return Err(GifAnimationScanError::Malformed);
    }
    let logical_screen_descriptor = encoded_gif_bytes
        .get(6..13)
        .ok_or(GifAnimationScanError::Malformed)?;
    let logical_screen_packed_fields = *logical_screen_descriptor
        .get(4)
        .ok_or(GifAnimationScanError::Malformed)?;
    let mut gif_byte_offset = 13usize;
    if logical_screen_packed_fields & 0x80 != 0 {
        let color_table_entry_count = 1usize
            .checked_shl(u32::from((logical_screen_packed_fields & 0x07) + 1))
            .ok_or(GifAnimationScanError::Malformed)?;
        let color_table_byte_count = color_table_entry_count
            .checked_mul(3)
            .ok_or(GifAnimationScanError::Malformed)?;
        take_gif_bytes(
            encoded_gif_bytes,
            &mut gif_byte_offset,
            color_table_byte_count,
        )?;
    }

    let mut image_frame_count = 0usize;
    let mut loop_policy =
        LoopPolicy::from_finite_playback_count(1).map_err(|_| GifAnimationScanError::Malformed)?;
    let mut has_loop_extension = false;
    loop {
        let block_introducer = *take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?
            .first()
            .ok_or(GifAnimationScanError::Malformed)?;
        match block_introducer {
            0x3b => {
                return Ok(GifAnimationScan {
                    is_animated: image_frame_count > 1 || has_loop_extension,
                    loop_policy,
                });
            }
            0x21 => {
                let extension_label = *take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?
                    .first()
                    .ok_or(GifAnimationScanError::Malformed)?;
                match extension_label {
                    0xff => {
                        if let Some(application_loop_policy) =
                            scan_gif_application_extension(encoded_gif_bytes, &mut gif_byte_offset)?
                        {
                            loop_policy = application_loop_policy;
                            has_loop_extension = true;
                        }
                    }
                    0x01 => {
                        let subblock_byte_count =
                            *take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?
                                .first()
                                .ok_or(GifAnimationScanError::Malformed)?;
                        if subblock_byte_count != 12 {
                            return Err(GifAnimationScanError::Malformed);
                        }
                        take_gif_bytes(
                            encoded_gif_bytes,
                            &mut gif_byte_offset,
                            usize::from(subblock_byte_count),
                        )?;
                        skip_gif_subblocks(encoded_gif_bytes, &mut gif_byte_offset)?;
                    }
                    0xf9 => {
                        let subblock_byte_count =
                            *take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?
                                .first()
                                .ok_or(GifAnimationScanError::Malformed)?;
                        if subblock_byte_count != 4 {
                            return Err(GifAnimationScanError::Malformed);
                        }
                        take_gif_bytes(
                            encoded_gif_bytes,
                            &mut gif_byte_offset,
                            usize::from(subblock_byte_count),
                        )?;
                        let block_terminator =
                            *take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?
                                .first()
                                .ok_or(GifAnimationScanError::Malformed)?;
                        if block_terminator != 0 {
                            return Err(GifAnimationScanError::Malformed);
                        }
                    }
                    _ => skip_gif_subblocks(encoded_gif_bytes, &mut gif_byte_offset)?,
                }
            }
            0x2c => {
                let image_descriptor = take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 9)?;
                let image_packed_fields = *image_descriptor
                    .get(8)
                    .ok_or(GifAnimationScanError::Malformed)?;
                if image_packed_fields & 0x80 != 0 {
                    let color_table_entry_count = 1usize
                        .checked_shl(u32::from((image_packed_fields & 0x07) + 1))
                        .ok_or(GifAnimationScanError::Malformed)?;
                    let color_table_byte_count = color_table_entry_count
                        .checked_mul(3)
                        .ok_or(GifAnimationScanError::Malformed)?;
                    take_gif_bytes(
                        encoded_gif_bytes,
                        &mut gif_byte_offset,
                        color_table_byte_count,
                    )?;
                }
                take_gif_bytes(encoded_gif_bytes, &mut gif_byte_offset, 1)?;
                skip_gif_subblocks(encoded_gif_bytes, &mut gif_byte_offset)?;
                image_frame_count = image_frame_count
                    .checked_add(1)
                    .ok_or(GifAnimationScanError::TooManyFrames)?;
                if image_frame_count > MAX_ANIMATION_FRAME_COUNT {
                    return Err(GifAnimationScanError::TooManyFrames);
                }
            }
            _ => return Err(GifAnimationScanError::Malformed),
        }
    }
}

fn scan_gif_application_extension(
    encoded_gif_bytes: &[u8],
    byte_offset: &mut usize,
) -> Result<Option<LoopPolicy>, GifAnimationScanError> {
    let identifier_byte_count = *take_gif_bytes(encoded_gif_bytes, byte_offset, 1)?
        .first()
        .ok_or(GifAnimationScanError::Malformed)?;
    let application_identifier = take_gif_bytes(
        encoded_gif_bytes,
        byte_offset,
        usize::from(identifier_byte_count),
    )?;
    let is_recognized_application =
        application_identifier == b"NETSCAPE2.0" || application_identifier == b"ANIMEXTS1.0";

    let mut first_application_data_block = None;
    loop {
        let subblock_byte_count = match take_gif_bytes(encoded_gif_bytes, byte_offset, 1) {
            Ok(subblock_header_bytes) => *subblock_header_bytes
                .first()
                .ok_or(GifAnimationScanError::Malformed)?,
            Err(_gif_scan_error) if is_recognized_application => {
                return Err(GifAnimationScanError::InvalidMetadata)
            }
            Err(gif_scan_error) => return Err(gif_scan_error),
        };
        if subblock_byte_count == 0 {
            break;
        }
        let application_block_bytes = match take_gif_bytes(
            encoded_gif_bytes,
            byte_offset,
            usize::from(subblock_byte_count),
        ) {
            Ok(application_block_bytes) => application_block_bytes,
            Err(_gif_scan_error) if is_recognized_application => {
                return Err(GifAnimationScanError::InvalidMetadata)
            }
            Err(gif_scan_error) => return Err(gif_scan_error),
        };
        if is_recognized_application && first_application_data_block.is_none() {
            first_application_data_block = Some(application_block_bytes);
        }
    }

    if !is_recognized_application {
        return Ok(None);
    }
    let first_application_data_block =
        first_application_data_block.ok_or(GifAnimationScanError::InvalidMetadata)?;
    if first_application_data_block.len() < 3 || first_application_data_block[0] != 1 {
        return Err(GifAnimationScanError::InvalidMetadata);
    }
    let repeat_count = u16::from_le_bytes([
        *first_application_data_block
            .get(1)
            .ok_or(GifAnimationScanError::InvalidMetadata)?,
        *first_application_data_block
            .get(2)
            .ok_or(GifAnimationScanError::InvalidMetadata)?,
    ]);
    if repeat_count == 0 {
        return Ok(Some(LoopPolicy::Infinite));
    }
    let total_playbacks = u32::from(repeat_count)
        .checked_add(1)
        .ok_or(GifAnimationScanError::InvalidMetadata)?;
    let loop_policy = LoopPolicy::from_finite_playback_count(total_playbacks)
        .map_err(|_| GifAnimationScanError::InvalidMetadata)?;
    Ok(Some(loop_policy))
}

fn take_gif_bytes<'a>(
    encoded_gif_bytes: &'a [u8],
    byte_offset: &mut usize,
    byte_length: usize,
) -> Result<&'a [u8], GifAnimationScanError> {
    let end_byte_offset = byte_offset
        .checked_add(byte_length)
        .ok_or(GifAnimationScanError::Malformed)?;
    let selected_bytes = encoded_gif_bytes
        .get(*byte_offset..end_byte_offset)
        .ok_or(GifAnimationScanError::Malformed)?;
    *byte_offset = end_byte_offset;
    Ok(selected_bytes)
}

fn skip_gif_subblocks(
    encoded_gif_bytes: &[u8],
    byte_offset: &mut usize,
) -> Result<(), GifAnimationScanError> {
    loop {
        let subblock_byte_count = *take_gif_bytes(encoded_gif_bytes, byte_offset, 1)?
            .first()
            .ok_or(GifAnimationScanError::Malformed)?;
        if subblock_byte_count == 0 {
            return Ok(());
        }
        take_gif_bytes(
            encoded_gif_bytes,
            byte_offset,
            usize::from(subblock_byte_count),
        )?;
    }
}

#[cfg(test)]
mod tests;
