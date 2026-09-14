//! Image model tests: serde validation and Kitty source rectangles.

use super::*;

fn build_test_image(image_pixel_width: u32, image_pixel_height: u32) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        pixel_width: image_pixel_width,
        pixel_height: image_pixel_height,
        rgba_bytes: vec![
            0;
            usize::try_from(image_pixel_width * image_pixel_height * 4)
                .expect("test image fits")
        ],
    })
}

fn build_image_record(protocol: GraphicsProtocol, display: ImageDisplay) -> ImageRecord {
    ImageRecord {
        protocol,
        image: build_test_image(8, 6),
        animation: None,
        action: ImageAction::Display,
        display,
        anchor: (2, 3),
    }
}

#[test]
fn decoded_image_round_trips_with_rgba_bytes() {
    let decoded_image = DecodedImage {
        pixel_width: 2,
        pixel_height: 1,
        rgba_bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
    };

    let serialized_decoded_image =
        serde_json::to_value(&decoded_image).expect("decoded image serializes");
    assert_eq!(
        serialized_decoded_image,
        serde_json::json!({
            "width": 2,
            "height": 1,
            "rgba": [1, 2, 3, 4, 5, 6, 7, 8]
        })
    );
    assert_eq!(
        serde_json::from_value::<DecodedImage>(serialized_decoded_image)
            .expect("decoded image deserializes"),
        decoded_image
    );
}

#[test]
fn decoded_image_rejects_zero_dimensions_and_mismatched_bytes() {
    let zero_pixel_width = serde_json::json!({"width": 0, "height": 1, "rgba": []});
    let mismatched_rgba_length = serde_json::json!({"width": 2, "height": 1, "rgba": [1, 2, 3, 4]});

    assert_eq!(
        serde_json::from_value::<DecodedImage>(zero_pixel_width)
            .expect_err("zero width must be rejected")
            .to_string(),
        "decoded image dimensions exceed graphics limits"
    );
    assert_eq!(
        serde_json::from_value::<DecodedImage>(mismatched_rgba_length)
            .expect_err("wrong RGBA length must be rejected")
            .to_string(),
        "decoded image RGBA length does not match its dimensions"
    );
}

#[test]
fn decoded_image_rejects_a_side_above_the_limit() {
    let invalid_image_json = serde_json::json!({
        "width": MAX_IMAGE_SIDE_PIXEL_COUNT as u32 + 1,
        "height": 1,
        "rgba": []
    });

    assert_eq!(
        serde_json::from_value::<DecodedImage>(invalid_image_json)
            .expect_err("an oversized side must be rejected")
            .to_string(),
        "decoded image dimensions exceed graphics limits"
    );
}

#[test]
fn compute_source_rect_uses_the_complete_image_for_non_kitty_records() {
    let image_record = build_image_record(GraphicsProtocol::Iterm2, ImageDisplay::default());

    assert_eq!(
        image_record
            .compute_source_rect()
            .expect("iTerm2 uses complete image"),
        (0, 0, 8, 6)
    );
}

#[test]
fn compute_source_rect_crops_and_clamps_kitty_pixel_dimensions() {
    let display = ImageDisplay {
        source_pixel_offset_x: Some(2),
        source_pixel_offset_y: Some(1),
        requested_width: Some(ImageDimension::Pixels(20)),
        requested_height: Some(ImageDimension::Pixels(3)),
        ..ImageDisplay::default()
    };
    let image_record = build_image_record(GraphicsProtocol::Kitty, display);

    assert_eq!(
        image_record
            .compute_source_rect()
            .expect("crop is inside image"),
        (2, 1, 6, 3)
    );
}

#[test]
fn compute_source_rect_rejects_a_kitty_origin_outside_the_image() {
    let display = ImageDisplay {
        source_pixel_offset_x: Some(8),
        ..ImageDisplay::default()
    };
    let image_record = build_image_record(GraphicsProtocol::Kitty, display);

    assert_eq!(
        image_record
            .compute_source_rect()
            .expect_err("origin is outside image"),
        ImagePlacementError::SourceOutOfBounds {
            source_x: 8,
            source_y: 0,
            source_pixel_width: 0,
            source_pixel_height: 6,
            image_pixel_width: 8,
            image_pixel_height: 6,
        }
    );
}
