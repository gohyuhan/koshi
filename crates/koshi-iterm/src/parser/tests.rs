//! Tests for iTerm2 image command parsing and multipart recovery.

use super::*;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_image::{DecodedImage, ImageAction, ImageDimension};

fn red_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");
    bytes
}

fn encode_base64_for_test(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn one_pixel_body(size: Option<&str>) -> Vec<u8> {
    let png = red_png();
    let encoded = encode_base64_for_test(&png);
    let size_field = size.map_or_else(String::new, |value| format!(";size={value}"));
    format!("File=inline=1;width=2;height=3px;preserveAspectRatio=0{size_field}:{encoded}")
        .into_bytes()
}

fn assert_red_image(result: &DecodedGraphics) {
    assert_eq!(result.image.width, 1);
    assert_eq!(result.image.height, 1);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn decodes_file_pixels_and_display_fields() {
    let mut state = None;
    let result = parse_iterm_command(&one_pixel_body(Some("73")), &mut state)
        .expect("valid iTerm2 file")
        .expect("one complete image");
    assert!(!result.query);
    assert_eq!(result.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(result.action, ImageAction::Display);
    assert_red_image(&result);
    assert_eq!(result.display.width, Some(ImageDimension::Cells(2)));
    assert_eq!(result.display.height, Some(ImageDimension::Pixels(3)));
    assert!(!result.display.preserve_aspect_ratio);
    assert!(state.is_none());
}

#[test]
fn size_hint_does_not_change_file_decode() {
    let values = [Some("0"), Some("1"), Some("999"), Some("4294967296"), None];
    for size in values {
        let mut state = None;
        let result = parse_iterm_command(&one_pixel_body(size), &mut state)
            .expect("size is a progress hint")
            .expect("one complete image");
        assert_red_image(&result);
        assert_eq!(result.image.rgba.len(), 4);
        assert!(state.is_none());
    }
}

#[test]
fn size_hint_accepts_every_decimal_digit_within_the_control_limit() {
    let png = red_png();
    let encoded = encode_base64_for_test(&png);
    let prefix = "inline=1;size=";
    let size = "9".repeat(MAX_GRAPHICS_CONTROL_BYTES - prefix.len());
    let body = format!("File={prefix}{size}:{encoded}");
    let mut state = None;

    let result = parse_iterm_command(body.as_bytes(), &mut state)
        .expect("the bounded decimal hint is valid")
        .expect("the image is complete");

    assert_red_image(&result);
    assert!(state.is_none());
}

#[test]
fn malformed_size_is_rejected_as_invalid_command() {
    let png = red_png();
    let encoded = encode_base64_for_test(&png);
    for size in ["", "not-a-number", "-1", "+1", "12x"] {
        let mut state = None;
        let body = format!("File=inline=1;size={size}:{encoded}");
        assert_eq!(
            parse_iterm_command(body.as_bytes(), &mut state),
            Err(GraphicsError::InvalidCommand {
                protocol: ITERM_PROTOCOL,
            }),
            "size={size:?}"
        );
        assert!(state.is_none());
    }
}

#[test]
fn multipart_parts_join_in_order_and_finish() {
    let png = red_png();
    let encoded = encode_base64_for_test(&png);
    let split = encoded.len() / 2;
    let mut state = None;
    assert!(parse_iterm_command(
        format!("MultipartFile=inline=1;width=1;height=1;size={}", png.len()).as_bytes(),
        &mut state,
    )
    .expect("multipart start")
    .is_none());
    assert!(parse_iterm_command(
        format!("FilePart={}", &encoded[..split]).as_bytes(),
        &mut state,
    )
    .expect("first part")
    .is_none());
    assert!(parse_iterm_command(
        format!("FilePart={}", &encoded[split..]).as_bytes(),
        &mut state,
    )
    .expect("second part")
    .is_none());
    let result = parse_iterm_command(b"FileEnd", &mut state)
        .expect("multipart end")
        .expect("complete image");
    assert_red_image(&result);
    assert!(state.is_none());
}

#[test]
fn size_hint_does_not_change_multipart_decode() {
    let png = red_png();
    let encoded = encode_base64_for_test(&png);
    for size in [Some("0"), Some("1"), Some("999"), None] {
        let mut state = None;
        let header = match size {
            Some(value) => format!("MultipartFile=inline=1;size={value}"),
            None => "MultipartFile=inline=1".to_string(),
        };
        assert_eq!(parse_iterm_command(header.as_bytes(), &mut state), Ok(None));
        assert_eq!(
            parse_iterm_command(format!("FilePart={encoded}").as_bytes(), &mut state),
            Ok(None)
        );
        let result = parse_iterm_command(b"FileEnd", &mut state)
            .expect("multipart completes")
            .expect("image is present");
        assert_red_image(&result);
        assert!(state.is_none());
    }
}

#[test]
fn large_size_hint_does_not_change_multipart_decode() {
    let encoded = encode_base64_for_test(&red_png());
    let mut state = None;

    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1;size=4294967296", &mut state,),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(format!("FilePart={encoded}").as_bytes(), &mut state),
        Ok(None)
    );
    let result = parse_iterm_command(b"FileEnd", &mut state)
        .expect("multipart completes")
        .expect("the image is present");

    assert_red_image(&result);
    assert!(state.is_none());
}

#[test]
fn signed_dimensions_are_canonicalized_to_renderable_units() {
    for (value, expected) in [
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
            parse_iterm_dimension(value.as_bytes()),
            Ok(expected),
            "{value}"
        );
    }
}

#[test]
fn duplicate_metadata_uses_the_final_value() {
    let encoded = encode_base64_for_test(&red_png());
    let mut state = None;
    let body = format!(
        "File=inline=0;width=2;height=4;inline=1;width=3;height=5;preserveAspectRatio=1;preserveAspectRatio=0:{encoded}"
    );

    let result = parse_iterm_command(body.as_bytes(), &mut state)
        .expect("the final metadata values are valid")
        .expect("the image is complete");

    assert_eq!(result.display.width, Some(ImageDimension::Cells(3)));
    assert_eq!(result.display.height, Some(ImageDimension::Cells(5)));
    assert!(!result.display.preserve_aspect_ratio);
    assert_red_image(&result);
}

#[test]
fn duplicate_inline_uses_the_final_disabled_value() {
    let encoded = encode_base64_for_test(&red_png());
    let mut state = None;
    let body = format!("File=inline=1;inline=0:{encoded}");

    assert_eq!(
        parse_iterm_command(body.as_bytes(), &mut state),
        Err(GraphicsError::UnsupportedAction {
            protocol: ITERM_PROTOCOL,
            action: "inline=0".to_string(),
        })
    );
    assert!(state.is_none());
}

#[test]
fn malformed_multipart_commands_preserve_state() {
    let mut state = None;
    assert_eq!(
        parse_iterm_command(b"FilePart=AAAA", &mut state),
        Err(GraphicsError::MultipartState)
    );
    assert!(state.is_none());
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut state),
        Ok(None)
    );
    let before = state.clone();
    assert_eq!(
        parse_iterm_command(b"FileEnd=bad", &mut state),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    assert_eq!(state, before);

    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut state),
        Err(GraphicsError::MultipartState)
    );
    assert_eq!(state, before);

    assert_eq!(
        parse_iterm_command(b"File=inline=1:bad$", &mut state),
        Err(GraphicsError::MultipartState)
    );
    assert_eq!(state, before);
}

#[test]
fn multipart_state_is_released_after_bad_image_and_accepts_next_file() {
    let mut state = None;
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut state),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(b"FilePart=not-base64", &mut state),
        Ok(None)
    );
    assert_eq!(
        parse_iterm_command(b"FileEnd", &mut state),
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Iterm2,
        })
    );
    assert!(state.is_none());

    let result = parse_iterm_command(&one_pixel_body(None), &mut state)
        .expect("next file remains parseable")
        .expect("next file completes");
    assert_red_image(&result);
    assert!(state.is_none());
}

#[test]
fn multipart_end_before_start_does_not_create_state() {
    let mut state = None;
    assert_eq!(
        parse_iterm_command(b"FileEnd", &mut state),
        Err(GraphicsError::MultipartState)
    );
    assert!(state.is_none());
}

#[test]
fn multipart_payload_limit_is_checked_before_append() {
    let mut state = None;
    assert_eq!(
        parse_iterm_command(b"MultipartFile=inline=1", &mut state),
        Ok(None)
    );
    let before = state.clone();
    let payload = vec![b'A'; MAX_GRAPHICS_TRANSFER_BYTES + 1];
    let command = [b"FilePart=".as_slice(), payload.as_slice()].concat();
    let error = parse_iterm_command(&command, &mut state)
        .expect_err("part exceeds the remaining transfer bound");
    assert_eq!(
        error,
        GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2
        }
    );
    assert_eq!(state, before);
}

#[test]
fn parsed_image_matches_shared_decoded_image_shape() {
    let image = DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    };
    let mut state = None;
    let result = parse_iterm_command(&one_pixel_body(None), &mut state)
        .expect("valid image")
        .expect("complete image");
    assert_eq!(result.image, image);
}
