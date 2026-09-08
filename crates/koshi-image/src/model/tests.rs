//! Image model tests: serde validation and Kitty source rectangles.

use super::*;

fn image(width: u32, height: u32) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        width,
        height,
        rgba: vec![0; usize::try_from(width * height * 4).expect("test image fits")],
    })
}

fn record(protocol: GraphicsProtocol, display: ImageDisplay) -> ImageRecord {
    ImageRecord {
        protocol,
        image: image(8, 6),
        animation: None,
        action: ImageAction::Display,
        display,
        anchor: (2, 3),
    }
}

#[test]
fn decoded_image_round_trips_with_rgba_bytes() {
    let source = DecodedImage {
        width: 2,
        height: 1,
        rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
    };

    let value = serde_json::to_value(&source).expect("decoded image serializes");
    assert_eq!(
        value,
        serde_json::json!({
            "width": 2,
            "height": 1,
            "rgba": [1, 2, 3, 4, 5, 6, 7, 8]
        })
    );
    assert_eq!(
        serde_json::from_value::<DecodedImage>(value).expect("decoded image deserializes"),
        source
    );
}

#[test]
fn decoded_image_rejects_zero_dimensions_and_mismatched_bytes() {
    let zero_width = serde_json::json!({"width": 0, "height": 1, "rgba": []});
    let mismatch = serde_json::json!({"width": 2, "height": 1, "rgba": [1, 2, 3, 4]});

    assert_eq!(
        serde_json::from_value::<DecodedImage>(zero_width)
            .expect_err("zero width must be rejected")
            .to_string(),
        "decoded image dimensions exceed graphics limits"
    );
    assert_eq!(
        serde_json::from_value::<DecodedImage>(mismatch)
            .expect_err("wrong RGBA length must be rejected")
            .to_string(),
        "decoded image RGBA length does not match its dimensions"
    );
}

#[test]
fn decoded_image_rejects_a_side_above_the_limit() {
    let value = serde_json::json!({
        "width": MAX_IMAGE_SIDE as u32 + 1,
        "height": 1,
        "rgba": []
    });

    assert_eq!(
        serde_json::from_value::<DecodedImage>(value)
            .expect_err("an oversized side must be rejected")
            .to_string(),
        "decoded image dimensions exceed graphics limits"
    );
}

#[test]
fn source_rect_uses_the_complete_image_for_non_kitty_records() {
    let record = record(GraphicsProtocol::Iterm2, ImageDisplay::default());

    assert_eq!(
        record.source_rect().expect("iTerm2 uses complete image"),
        (0, 0, 8, 6)
    );
}

#[test]
fn source_rect_crops_and_clamps_kitty_pixel_dimensions() {
    let display = ImageDisplay {
        source_offset_x: Some(2),
        source_offset_y: Some(1),
        width: Some(ImageDimension::Pixels(20)),
        height: Some(ImageDimension::Pixels(3)),
        ..ImageDisplay::default()
    };
    let record = record(GraphicsProtocol::Kitty, display);

    assert_eq!(
        record.source_rect().expect("crop is inside image"),
        (2, 1, 6, 3)
    );
}

#[test]
fn source_rect_rejects_a_kitty_origin_outside_the_image() {
    let display = ImageDisplay {
        source_offset_x: Some(8),
        ..ImageDisplay::default()
    };
    let record = record(GraphicsProtocol::Kitty, display);

    assert_eq!(
        record.source_rect().expect_err("origin is outside image"),
        ImagePlacementError::SourceOutOfBounds {
            x: 8,
            y: 0,
            width: 0,
            height: 6,
            image_width: 8,
            image_height: 6,
        }
    );
}
