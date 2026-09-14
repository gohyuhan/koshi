//! Tests for iTerm2 image command parsing and multipart recovery.

use super::*;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_image::{DecodedImage, ImageAction, ImageDimension};

fn build_red_png() -> Vec<u8> {
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");
    png_bytes
}

fn encode_base64_for_test(source_bytes: &[u8]) -> String {
    STANDARD.encode(source_bytes)
}

#[test]
fn command_classifiers_match_complete_and_partial_graphics_names() {
    for (command_body, expected_is_graphics) in [
        (b"".as_slice(), true),
        (b"File", true),
        (b"File=inline=1", true),
        (b"MultipartF", true),
        (b"FilePart=AAAA", true),
        (b"FileEnd", true),
        (b"FileExtra", false),
        (b"Other", false),
    ] {
        assert_eq!(
            can_iterm_command_be_graphics(command_body),
            expected_is_graphics,
            "{command_body:?}"
        );
    }

    for (command_body, expected_is_graphics) in [
        (b"".as_slice(), true),
        (b"File", true),
        (b"MultipartFile=inline=1", true),
        (b"FilePart=AAAA", true),
        (b"FileEnd", true),
        (b"FileExtra", false),
        (b"Other", false),
    ] {
        assert_eq!(
            is_iterm_graphics_command(command_body),
            expected_is_graphics,
            "{command_body:?}"
        );
    }
}

#[test]
fn payload_classifier_waits_for_image_data() {
    for (command_body, expected_is_payload_started) in [
        (b"".as_slice(), false),
        (b"File", false),
        (b"File=inline=1", false),
        (b"File=inline=1:", true),
        (b"MultipartFile=inline=1", false),
        (b"MultipartFile=inline=1:", true),
        (b"FilePart=", true),
        (b"FilePartExtra=", false),
    ] {
        assert_eq!(
            is_iterm_payload_started(command_body),
            expected_is_payload_started,
            "{command_body:?}"
        );
    }
}

fn build_one_pixel_command_body(size_hint: Option<&str>) -> Vec<u8> {
    let red_png_bytes = build_red_png();
    let encoded_png_base64 = encode_base64_for_test(&red_png_bytes);
    let size_parameter = size_hint.map_or_else(String::new, |dimension_text| {
        format!(";size={dimension_text}")
    });
    format!(
        "File=inline=1;width=2;height=3px;preserveAspectRatio=0{size_parameter}:{encoded_png_base64}"
    )
    .into_bytes()
}

fn assert_decoded_red_image(decoded_graphics: &DecodedGraphics) {
    assert_eq!(decoded_graphics.image.pixel_width, 1);
    assert_eq!(decoded_graphics.image.pixel_height, 1);
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn decodes_file_pixels_and_display_fields() {
    let mut multipart_transfer = None;
    let decoded_graphics = parse_iterm_command(
        &build_one_pixel_command_body(Some("73")),
        &mut multipart_transfer,
    )
    .expect("valid iTerm2 file")
    .expect("one complete image");
    assert!(!decoded_graphics.is_query);
    assert_eq!(decoded_graphics.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(decoded_graphics.action, ImageAction::Display);
    assert_decoded_red_image(&decoded_graphics);
    assert_eq!(
        decoded_graphics.display.requested_width,
        Some(ImageDimension::Cells(2))
    );
    assert_eq!(
        decoded_graphics.display.requested_height,
        Some(ImageDimension::Pixels(3))
    );
    assert!(!decoded_graphics.display.is_aspect_ratio_preserved);
    assert!(multipart_transfer.is_none());
}

#[test]
fn size_hint_does_not_change_file_decode() {
    let size_hints = [Some("0"), Some("1"), Some("999"), Some("4294967296"), None];
    for size_hint in size_hints {
        let mut multipart_transfer = None;
        let decoded_graphics = parse_iterm_command(
            &build_one_pixel_command_body(size_hint),
            &mut multipart_transfer,
        )
        .expect("size is a progress hint")
        .expect("one complete image");
        assert_decoded_red_image(&decoded_graphics);
        assert_eq!(decoded_graphics.image.rgba_bytes.len(), 4);
        assert!(multipart_transfer.is_none());
    }
}

#[test]
fn size_hint_accepts_every_decimal_digit_within_the_control_limit() {
    let red_png_bytes = build_red_png();
    let encoded_png_base64 = encode_base64_for_test(&red_png_bytes);
    let size_prefix = "inline=1;size=";
    let size_text = "9".repeat(MAX_GRAPHICS_CONTROL_BYTE_COUNT - size_prefix.len());
    let command_body = format!("File={size_prefix}{size_text}:{encoded_png_base64}");
    let mut multipart_transfer = None;

    let decoded_graphics = parse_iterm_command(command_body.as_bytes(), &mut multipart_transfer)
        .expect("the bounded decimal hint is valid")
        .expect("the image is complete");

    assert_decoded_red_image(&decoded_graphics);
    assert!(multipart_transfer.is_none());
}

#[test]
fn malformed_size_is_rejected_as_invalid_command() {
    let red_png_bytes = build_red_png();
    let encoded_png_base64 = encode_base64_for_test(&red_png_bytes);
    for size_text in ["", "not-a-number", "-1", "+1", "12x"] {
        let mut multipart_transfer = None;
        let command_body = format!("File=inline=1;size={size_text}:{encoded_png_base64}");
        assert_eq!(
            parse_iterm_command(command_body.as_bytes(), &mut multipart_transfer),
            Err(GraphicsError::InvalidCommand {
                protocol: ITERM2_PROTOCOL,
            }),
            "size={size_text:?}"
        );
        assert!(multipart_transfer.is_none());
    }
}

#[test]
fn multipart_parts_join_in_order_and_finish() {
    let red_png_bytes = build_red_png();
    let encoded_png_base64 = encode_base64_for_test(&red_png_bytes);
    let payload_split_index = encoded_png_base64.len() / 2;
    let mut multipart_transfer = None;
    assert!(parse_iterm_command(
        format!(
            "MultipartFile=inline=1;width=1;height=1;size={}",
            red_png_bytes.len()
        )
        .as_bytes(),
        &mut multipart_transfer,
    )
    .expect("multipart start")
    .is_none());
    assert!(parse_iterm_command(
        format!("FilePart={}", &encoded_png_base64[..payload_split_index]).as_bytes(),
        &mut multipart_transfer,
    )
    .expect("first part")
    .is_none());
    assert!(parse_iterm_command(
        format!("FilePart={}", &encoded_png_base64[payload_split_index..]).as_bytes(),
        &mut multipart_transfer,
    )
    .expect("second part")
    .is_none());
    let decoded_graphics = parse_iterm_command(b"FileEnd", &mut multipart_transfer)
        .expect("multipart end")
        .expect("complete image");
    assert_decoded_red_image(&decoded_graphics);
    assert!(multipart_transfer.is_none());
}

#[test]
fn size_hint_does_not_change_multipart_decode() {
    let red_png_bytes = build_red_png();
    let encoded_png_base64 = encode_base64_for_test(&red_png_bytes);
    for size_hint in [Some("0"), Some("1"), Some("999"), None] {
        let mut multipart_transfer = None;
        let multipart_header = match size_hint {
            Some(dimension_text) => format!("MultipartFile=inline=1;size={dimension_text}"),
            None => "MultipartFile=inline=1".to_string(),
        };
        assert_eq!(
            parse_iterm_command(multipart_header.as_bytes(), &mut multipart_transfer),
            Ok(None)
        );
        assert_eq!(
            parse_iterm_command(
                format!("FilePart={encoded_png_base64}").as_bytes(),
                &mut multipart_transfer
            ),
            Ok(None)
        );
        let decoded_graphics = parse_iterm_command(b"FileEnd", &mut multipart_transfer)
            .expect("multipart completes")
            .expect("image is present");
        assert_decoded_red_image(&decoded_graphics);
        assert!(multipart_transfer.is_none());
    }
}

#[test]
fn large_size_hint_does_not_change_multipart_decode() {
    let encoded_png_base64 = encode_base64_for_test(&build_red_png());
    let mut multipart_transfer = None;

    assert_eq!(
        parse_iterm_command(
            b"MultipartFile=inline=1;size=4294967296",
            &mut multipart_transfer,
        ),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(
            format!("FilePart={encoded_png_base64}").as_bytes(),
            &mut multipart_transfer
        ),
        Ok(None)
    );
    let decoded_graphics = parse_iterm_command(b"FileEnd", &mut multipart_transfer)
        .expect("multipart completes")
        .expect("the image is present");

    assert_decoded_red_image(&decoded_graphics);
    assert!(multipart_transfer.is_none());
}

#[test]
fn signed_dimensions_are_canonicalized_to_renderable_units() {
    for (dimension_text, expected_dimension) in [
        ("-2", ImageDimension::Cells(1)),
        ("0", ImageDimension::Cells(1)),
        ("+2", ImageDimension::Cells(2)),
        ("7", ImageDimension::Cells(7)),
        ("-2px", ImageDimension::Pixels(1)),
        ("0px", ImageDimension::Pixels(1)),
        ("+2px", ImageDimension::Pixels(2)),
        ("7px", ImageDimension::Pixels(7)),
        ("-2%", ImageDimension::Cells(1)),
        ("0%", ImageDimension::Cells(1)),
        ("1%", ImageDimension::Percent(1)),
        ("100%", ImageDimension::Percent(100)),
        ("250%", ImageDimension::Percent(100)),
        (
            "999999999999999999999999999999%",
            ImageDimension::Percent(100),
        ),
    ] {
        assert_eq!(
            parse_iterm_dimension(dimension_text.as_bytes()),
            Ok(expected_dimension),
            "{dimension_text}"
        );
    }
}

#[test]
fn duplicate_metadata_uses_the_final_value() {
    let encoded_png_base64 = encode_base64_for_test(&build_red_png());
    let mut multipart_transfer = None;
    let command_body = format!(
        "File=inline=0;width=2;height=4;inline=1;width=3;height=5;preserveAspectRatio=1;preserveAspectRatio=0:{encoded_png_base64}"
    );

    let decoded_graphics = parse_iterm_command(command_body.as_bytes(), &mut multipart_transfer)
        .expect("the final metadata values are valid")
        .expect("the image is complete");

    assert_eq!(
        decoded_graphics.display.requested_width,
        Some(ImageDimension::Cells(3))
    );
    assert_eq!(
        decoded_graphics.display.requested_height,
        Some(ImageDimension::Cells(5))
    );
    assert!(!decoded_graphics.display.is_aspect_ratio_preserved);
    assert_decoded_red_image(&decoded_graphics);
}

#[test]
fn duplicate_inline_uses_the_final_disabled_value() {
    let encoded_png_base64 = encode_base64_for_test(&build_red_png());
    let mut multipart_transfer = None;
    let command_body = format!("File=inline=1;inline=0:{encoded_png_base64}");

    assert_eq!(
        parse_iterm_command(command_body.as_bytes(), &mut multipart_transfer),
        Err(GraphicsError::UnsupportedAction {
            protocol: ITERM2_PROTOCOL,
            action: "inline=0".to_string(),
        })
    );
    assert!(multipart_transfer.is_none());
}

#[test]
fn malformed_multipart_commands_preserve_multipart_transfer() {
    let mut multipart_transfer = None;
    assert_eq!(
        parse_iterm_command(b"FilePart=AAAA", &mut multipart_transfer),
        Err(GraphicsError::MultipartState)
    );
    assert!(multipart_transfer.is_none());
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut multipart_transfer),
        Ok(None)
    );
    let multipart_transfer_before = multipart_transfer.clone();
    assert_eq!(
        parse_iterm_command(b"FileEnd=bad", &mut multipart_transfer),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    assert_eq!(multipart_transfer, multipart_transfer_before);

    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut multipart_transfer),
        Err(GraphicsError::MultipartState)
    );
    assert_eq!(multipart_transfer, multipart_transfer_before);

    assert_eq!(
        parse_iterm_command(b"File=inline=1:bad$", &mut multipart_transfer),
        Err(GraphicsError::MultipartState)
    );
    assert_eq!(multipart_transfer, multipart_transfer_before);
}

#[test]
fn multipart_transfer_is_released_after_bad_image_and_accepts_next_file() {
    let mut multipart_transfer = None;
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut multipart_transfer),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(b"FilePart=not-base64", &mut multipart_transfer),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(b"FileEnd", &mut multipart_transfer),
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Iterm2,
        })
    );
    assert!(multipart_transfer.is_none());

    let decoded_graphics =
        parse_iterm_command(&build_one_pixel_command_body(None), &mut multipart_transfer)
            .expect("next file remains parseable")
            .expect("next file completes");
    assert_decoded_red_image(&decoded_graphics);
    assert!(multipart_transfer.is_none());
}

#[test]
fn multipart_end_before_start_does_not_create_transfer() {
    let mut multipart_transfer = None;
    assert_eq!(
        parse_iterm_command(b"FileEnd", &mut multipart_transfer),
        Err(GraphicsError::MultipartState)
    );
    assert!(multipart_transfer.is_none());
}

#[test]
fn multipart_payload_limit_is_checked_before_append() {
    let mut multipart_transfer = None;
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut multipart_transfer),
        Ok(None)
    );
    let multipart_transfer_before = multipart_transfer.clone();
    let payload_bytes = vec![b'A'; MAX_GRAPHICS_TRANSFER_BYTE_COUNT + 1];
    let command_body = [b"FilePart=".as_slice(), payload_bytes.as_slice()].concat();
    let parse_error = parse_iterm_command(&command_body, &mut multipart_transfer)
        .expect_err("part exceeds the remaining transfer bound");
    assert_eq!(
        parse_error,
        GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2
        }
    );
    assert_eq!(multipart_transfer, multipart_transfer_before);
}

#[test]
fn parsed_image_matches_shared_decoded_image_shape() {
    let expected_image = DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    };
    let mut multipart_transfer = None;
    let decoded_graphics =
        parse_iterm_command(&build_one_pixel_command_body(None), &mut multipart_transfer)
            .expect("valid image")
            .expect("complete image");
    assert_eq!(decoded_graphics.image, expected_image);
}
