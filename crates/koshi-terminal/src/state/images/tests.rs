//! Tests for retained image animation state and frame timing.

use std::sync::Arc;
use std::time::Duration;

use koshi_core::process::PtySize;
use koshi_image::{AnimationFrame, DecodedAnimation, DecodedImage, FrameDelay, LoopPolicy};

use super::*;
use crate::graphics::{GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord};

fn build_animated_image_record(loop_policy: LoopPolicy, delay_milliseconds: u32) -> ImageRecord {
    let first_frame_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    });
    let second_frame_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![0, 255, 0, 255],
    });
    let frame_delay =
        FrameDelay::from_millisecond_ratio(delay_milliseconds, 1).expect("the test delay is valid");
    let animation = Arc::new(
        DecodedAnimation::from_frames_and_loop_policy(
            vec![
                AnimationFrame::from_image_and_delay(first_frame_image, frame_delay)
                    .expect("the first frame is valid"),
                AnimationFrame::from_image_and_delay(second_frame_image, frame_delay)
                    .expect("the second frame is valid"),
            ],
            loop_policy,
        )
        .expect("the animation is valid"),
    );
    ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: animation.list_frames()[0].clone_decoded_image(),
        animation: Some(animation),
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    }
}

#[test]
fn animation_frame_changes_after_its_delay() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).expect("cell size"),
    );
    terminal_state
        .apply_image_record(&build_animated_image_record(LoopPolicy::Infinite, 10))
        .expect("the animated image fits");

    assert_eq!(
        terminal_state.get_next_image_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(!terminal_state.advance_image_animations(Duration::from_millis(9)));
    assert_eq!(
        terminal_state.list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
    assert!(terminal_state.advance_image_animations(Duration::from_millis(1)));
    assert_eq!(
        terminal_state.list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [0, 255, 0, 255]
    );
}

#[test]
fn zero_delay_animation_uses_the_normal_render_interval() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).expect("cell size"),
    );
    terminal_state
        .apply_image_record(&build_animated_image_record(LoopPolicy::Infinite, 0))
        .expect("the animated image fits");

    assert_eq!(
        terminal_state.get_next_image_animation_delay(),
        Some(Duration::from_millis(8))
    );
}

#[test]
fn finite_animation_stops_on_its_last_frame() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).expect("cell size"),
    );
    terminal_state
        .apply_image_record(&build_animated_image_record(
            LoopPolicy::from_finite_playback_count(1).expect("one playback is valid"),
            10,
        ))
        .expect("the animated image fits");

    assert!(terminal_state.advance_image_animations(Duration::from_millis(10)));
    assert_eq!(
        terminal_state.get_next_image_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(!terminal_state.advance_image_animations(Duration::from_millis(10)));
    assert_eq!(
        terminal_state.list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [0, 255, 0, 255]
    );
    assert_eq!(terminal_state.get_next_image_animation_delay(), None);
}

#[test]
fn gapless_kitty_style_frames_are_skipped_without_a_visible_intermediate_frame() {
    let first_frame_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    });
    let gapless_frame_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![0, 0, 255, 255],
    });
    let final_frame_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![0, 255, 0, 255],
    });
    let animation = Arc::new(
        DecodedAnimation::from_frames_and_loop_policy(
            vec![
                AnimationFrame::from_image_and_delay(
                    first_frame_image,
                    FrameDelay::from_millisecond_ratio(10, 1).expect("delay is valid"),
                )
                .expect("the first frame is valid"),
                AnimationFrame::from_gapless_image(gapless_frame_image)
                    .expect("the gapless frame is valid"),
                AnimationFrame::from_image_and_delay(
                    final_frame_image,
                    FrameDelay::from_millisecond_ratio(10, 1).expect("delay is valid"),
                )
                .expect("the last frame is valid"),
            ],
            LoopPolicy::Infinite,
        )
        .expect("the animation is valid"),
    );
    let image_record = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: animation.list_frames()[0].clone_decoded_image(),
        animation: Some(animation),
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).expect("cell size"),
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    assert_eq!(
        terminal_state.get_next_image_animation_delay(),
        Some(Duration::from_millis(10))
    );
    assert!(terminal_state.advance_image_animations(Duration::from_millis(10)));
    assert_eq!(
        terminal_state.list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [0, 255, 0, 255]
    );
}

#[test]
fn image_storage_counts_shared_animation_and_raster_pixels_once() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).expect("cell size"),
    );
    terminal_state
        .apply_image_record(&build_animated_image_record(LoopPolicy::Infinite, 10))
        .expect("the animated image fits");
    let image_content = Arc::clone(&terminal_state.primary_image_placements[0].image_content);
    terminal_state.primary_image_placements[0].raster =
        Some(Arc::clone(&image_content.decoded_image));

    assert_eq!(terminal_state.get_image_storage_byte_count(), 8);
    assert!(Arc::ptr_eq(
        &image_content.decoded_image,
        &image_content
            .animation
            .as_ref()
            .expect("animation")
            .list_frames()[0]
            .clone_decoded_image()
    ));

    let restored_terminal_state: TerminalState = serde_json::from_value(
        serde_json::to_value(&terminal_state).expect("the terminal state serializes"),
    )
    .expect("the terminal state restores");
    let restored_image_placement = &restored_terminal_state.primary_image_placements[0];
    assert!(Arc::ptr_eq(
        &restored_image_placement.image_content.decoded_image,
        &restored_image_placement
            .image_content
            .animation
            .as_ref()
            .expect("animation")
            .list_frames()[0]
            .clone_decoded_image()
    ));
    assert_eq!(restored_image_placement.raster, None);
}

#[test]
fn shared_animation_pixels_fill_the_storage_limit_once() {
    let frame_storage_byte_count = MAX_IMAGE_STORAGE_BYTE_COUNT / 2;
    let frame_pixel_width = 8_192;
    let frame_pixel_height =
        u32::try_from(frame_storage_byte_count / 4 / frame_pixel_width).expect("height fits");
    let first_frame_image = Arc::new(DecodedImage {
        pixel_width: frame_pixel_width as u32,
        pixel_height: frame_pixel_height,
        rgba_bytes: vec![1; frame_storage_byte_count],
    });
    let second_frame_image = Arc::new(DecodedImage {
        pixel_width: frame_pixel_width as u32,
        pixel_height: frame_pixel_height,
        rgba_bytes: vec![2; frame_storage_byte_count],
    });
    let animation = Arc::new(
        DecodedAnimation::from_frames_and_loop_policy(
            vec![
                AnimationFrame::from_image_and_delay(
                    Arc::clone(&first_frame_image),
                    FrameDelay::from_millisecond_ratio(10, 1).expect("delay"),
                )
                .expect("first frame"),
                AnimationFrame::from_image_and_delay(
                    Arc::clone(&second_frame_image),
                    FrameDelay::from_millisecond_ratio(10, 1).expect("delay"),
                )
                .expect("second frame"),
            ],
            LoopPolicy::Infinite,
        )
        .expect("the animation fills the image limit"),
    );
    let image_record = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: first_frame_image,
        animation: Some(animation),
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            image_id: Some(1),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 2,
    });

    terminal_state
        .apply_image_record(&image_record)
        .expect("shared current-frame pixels are charged once");
    assert_eq!(
        terminal_state.get_image_storage_byte_count(),
        MAX_IMAGE_STORAGE_BYTE_COUNT
    );
    let terminal_state_before_additional_image = terminal_state.clone();
    let additional_image_record = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: Arc::new(DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![3; 4],
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
        terminal_state.apply_image_record(&additional_image_record),
        Err(ImagePlacementError::StorageLimit {
            used_byte_count: MAX_IMAGE_STORAGE_BYTE_COUNT,
            requested_byte_count: 4,
            byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
        })
    );
    assert_eq!(terminal_state, terminal_state_before_additional_image);
}
