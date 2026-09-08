//! Tests for retained image animation state and frame timing.

use std::sync::Arc;
use std::time::Duration;

use koshi_core::process::PtySize;
use koshi_image::{AnimationFrame, DecodedAnimation, DecodedImage, FrameDelay, LoopPolicy};

use super::*;
use crate::graphics::{GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord};

fn animated_record(loop_policy: LoopPolicy, delay_ms: u32) -> ImageRecord {
    let first = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    });
    let second = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![0, 255, 0, 255],
    });
    let delay = FrameDelay::new(delay_ms, 1).expect("the test delay is valid");
    let animation = Arc::new(
        DecodedAnimation::new(
            vec![
                AnimationFrame::new(first, delay).expect("the first frame is valid"),
                AnimationFrame::new(second, delay).expect("the second frame is valid"),
            ],
            loop_policy,
        )
        .expect("the animation is valid"),
    );
    ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: animation.frames()[0].image_shared(),
        animation: Some(animation),
        action: ImageAction::Display,
        display: ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    }
}

#[test]
fn animation_frame_changes_after_its_delay() {
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).expect("cell size"));
    state
        .apply_image_record(&animated_record(LoopPolicy::Infinite, 10))
        .expect("the animated image fits");

    assert_eq!(
        state.next_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(!state.advance_animations(Duration::from_millis(9)));
    assert_eq!(
        state.image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
    assert!(state.advance_animations(Duration::from_millis(1)));
    assert_eq!(
        state.image_placements()[0].record().image.rgba,
        [0, 255, 0, 255]
    );
}

#[test]
fn zero_delay_animation_uses_the_normal_render_interval() {
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).expect("cell size"));
    state
        .apply_image_record(&animated_record(LoopPolicy::Infinite, 0))
        .expect("the animated image fits");

    assert_eq!(state.next_animation_delay(), Some(Duration::from_millis(8)));
}

#[test]
fn finite_animation_stops_on_its_last_frame() {
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).expect("cell size"));
    state
        .apply_image_record(&animated_record(
            LoopPolicy::finite(1).expect("one playback is valid"),
            10,
        ))
        .expect("the animated image fits");

    assert!(state.advance_animations(Duration::from_millis(10)));
    assert_eq!(
        state.next_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(!state.advance_animations(Duration::from_millis(10)));
    assert_eq!(
        state.image_placements()[0].record().image.rgba,
        [0, 255, 0, 255]
    );
    assert_eq!(state.next_animation_delay(), None);
}

#[test]
fn gapless_kitty_style_frames_are_skipped_without_a_visible_intermediate_frame() {
    let first = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    });
    let gapless = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 255, 255],
    });
    let last = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![0, 255, 0, 255],
    });
    let animation = Arc::new(
        DecodedAnimation::new(
            vec![
                AnimationFrame::new(first, FrameDelay::new(10, 1).expect("delay is valid"))
                    .expect("the first frame is valid"),
                AnimationFrame::new_gapless(gapless).expect("the gapless frame is valid"),
                AnimationFrame::new(last, FrameDelay::new(10, 1).expect("delay is valid"))
                    .expect("the last frame is valid"),
            ],
            LoopPolicy::Infinite,
        )
        .expect("the animation is valid"),
    );
    let record = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: animation.frames()[0].image_shared(),
        animation: Some(animation),
        action: ImageAction::Display,
        display: ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).expect("cell size"));
    state.apply_image_record(&record).expect("the image fits");

    assert_eq!(
        state.next_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(state.advance_animations(Duration::from_millis(10)));
    assert_eq!(
        state.image_placements()[0].record().image.rgba,
        [0, 255, 0, 255]
    );
}

#[test]
fn image_storage_counts_shared_animation_and_raster_pixels_once() {
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).expect("cell size"));
    state
        .apply_image_record(&animated_record(LoopPolicy::Infinite, 10))
        .expect("the animated image fits");
    let content = Arc::clone(&state.primary_image_placements[0].content);
    state.primary_image_placements[0].raster = Some(Arc::clone(&content.image));

    assert_eq!(state.image_storage_bytes(), 8);
    assert!(Arc::ptr_eq(
        &content.image,
        &content.animation.as_ref().expect("animation").frames()[0].image_shared()
    ));

    let restored: TerminalState = serde_json::from_value(
        serde_json::to_value(&state).expect("the terminal state serializes"),
    )
    .expect("the terminal state restores");
    let restored = &restored.primary_image_placements[0];
    assert!(Arc::ptr_eq(
        &restored.content.image,
        &restored
            .content
            .animation
            .as_ref()
            .expect("animation")
            .frames()[0]
            .image_shared()
    ));
    assert_eq!(restored.raster, None);
}

#[test]
fn shared_animation_pixels_fill_the_storage_limit_once() {
    let frame_bytes = MAX_IMAGE_STORAGE_BYTES / 2;
    let width = 8_192;
    let height = u32::try_from(frame_bytes / 4 / width).expect("height fits");
    let first = Arc::new(DecodedImage {
        width: width as u32,
        height,
        rgba: vec![1; frame_bytes],
    });
    let second = Arc::new(DecodedImage {
        width: width as u32,
        height,
        rgba: vec![2; frame_bytes],
    });
    let animation = Arc::new(
        DecodedAnimation::new(
            vec![
                AnimationFrame::new(Arc::clone(&first), FrameDelay::new(10, 1).expect("delay"))
                    .expect("first frame"),
                AnimationFrame::new(Arc::clone(&second), FrameDelay::new(10, 1).expect("delay"))
                    .expect("second frame"),
            ],
            LoopPolicy::Infinite,
        )
        .expect("the animation fills the image limit"),
    );
    let record = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: first,
        animation: Some(animation),
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            image_id: Some(1),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 2 });

    state
        .apply_image_record(&record)
        .expect("shared current-frame pixels are charged once");
    assert_eq!(state.image_storage_bytes(), MAX_IMAGE_STORAGE_BYTES);
    let before = state.clone();
    let extra = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: Arc::new(DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![3; 4],
        }),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay {
            image_id: Some(2),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    assert_eq!(
        state.apply_image_record(&extra),
        Err(ImagePlacementError::StorageLimit {
            used_bytes: MAX_IMAGE_STORAGE_BYTES,
            requested_bytes: 4,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        })
    );
    assert_eq!(state, before);
}
