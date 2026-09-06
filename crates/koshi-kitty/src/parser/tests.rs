//! Tests for Kitty payload parsing and image decoding.

use super::*;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;

fn parse_chunk(body: &[u8]) -> Result<KittyChunk, GraphicsError> {
    let mut parser = KittyParser::new();
    for &byte in body {
        parser.feed(byte).expect("the Kitty body is valid");
    }
    parser.finish()
}

fn red_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the PNG encodes");
    bytes
}

fn red_jpeg() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0], 1, 1, image::ColorType::Rgb8.into())
        .expect("the JPEG encodes");
    bytes
}

fn complete(body: &[u8]) -> Result<DecodedGraphics, GraphicsError> {
    let chunk = parse_chunk(body)?;
    match start_transfer(chunk)? {
        KittyTransferOutcome::Complete(image) => Ok(image),
        KittyTransferOutcome::Pending(_) => {
            panic!("the body unexpectedly starts a multipart transfer")
        }
    }
}

#[test]
fn parser_exposes_bounded_header_and_payload_accessors() {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let body = format!("Gf=32,s=1,v=1;{payload}");
    let mut parser = KittyParser::new();
    for &byte in body.as_bytes() {
        parser.feed(byte).expect("the Kitty body is valid");
    }

    assert!(!parser.is_ignored());
    assert!(!parser.is_escaped());
    assert!(parser.has_header());
    assert_eq!(parser.header(), b"Gf=32,s=1,v=1");
    assert_eq!(parser.payload(), payload.as_bytes());
    assert_eq!(parser.payload_len(), payload.len());
}

#[test]
fn parser_appends_payload_runs_with_the_same_limit() {
    let mut parser = KittyParser::new();
    parser.feed(b'G').expect("the Kitty introducer is valid");
    parser.feed(b';').expect("the empty control data is valid");
    parser
        .append_payload(&vec![b'A'; MAX_KITTY_CHUNK_BYTES])
        .expect("one chunk stays within the bound");
    assert_eq!(parser.payload_len(), MAX_KITTY_CHUNK_BYTES);
    assert_eq!(
        parser.append_payload(b"A"),
        Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn raw_rgba_transfer_decodes_exact_pixels() {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let body = format!("Gf=32,s=1,v=1;{payload}");

    let image = complete(body.as_bytes()).expect("the RGBA transfer decodes");

    assert_eq!(image.protocol, KITTY_PROTOCOL);
    assert_eq!(image.action, ImageAction::Transmit);
    assert_eq!(image.image.width, 1);
    assert_eq!(image.image.height, 1);
    assert_eq!(image.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn png_transfer_decodes_png_data() {
    let payload = STANDARD.encode(red_png());
    let body = format!("Gf=100;{payload}");

    let image = complete(body.as_bytes()).expect("the PNG transfer decodes");

    assert_eq!(image.image.width, 1);
    assert_eq!(image.image.height, 1);
    assert_eq!(image.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn png_transfer_rejects_mislabeled_jpeg_bytes() {
    let payload = STANDARD.encode(red_jpeg());
    let body = format!("Gf=100;{payload}");

    assert_eq!(
        complete(body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn png_compression_requires_declared_uncompressed_size() {
    assert_eq!(
        complete(b"Gf=100,o=z;AAAA"),
        Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn oversized_raw_dimensions_are_rejected_before_payload_decode() {
    assert_eq!(
        complete(b"Gf=32,s=4097,v=4097;AAAA"),
        Err(GraphicsError::ImageTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn multipart_transfer_decodes_only_after_the_final_chunk() {
    let encoded = STANDARD.encode([255, 0, 0, 255]);
    let first = parse_chunk(format!("Gf=32,s=1,v=1,m=1;{}", &encoded[..4]).as_bytes())
        .expect("the first chunk is valid");
    let second = parse_chunk(format!("Gm=0;{}", &encoded[4..]).as_bytes())
        .expect("the second chunk is valid");
    let KittyTransferOutcome::Pending(transfer) = start_transfer(first).expect("first chunk")
    else {
        panic!("the first chunk completed unexpectedly")
    };

    let KittyTransferOutcome::Complete(image) = transfer
        .accept_chunk(second)
        .expect("the final chunk completes")
    else {
        panic!("the final chunk remained pending")
    };
    assert_eq!(image.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn continuation_rejects_non_chunk_control_fields() {
    let encoded = STANDARD.encode([255, 0, 0, 255]);
    let first = parse_chunk(format!("Gf=32,s=1,v=1,m=1;{}", &encoded[..4]).as_bytes())
        .expect("the first chunk is valid");
    let second = parse_chunk(format!("Gf=32,m=0;{}", &encoded[4..]).as_bytes())
        .expect("the second chunk is valid");
    let KittyTransferOutcome::Pending(transfer) = start_transfer(first).expect("first chunk")
    else {
        panic!("the first chunk completed unexpectedly")
    };

    let result = transfer.accept_chunk(second);
    match result {
        Err(error) => assert_eq!(
            error,
            GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            }
        ),
        Ok(_) => panic!("a non-chunk control field was accepted"),
    }
}
