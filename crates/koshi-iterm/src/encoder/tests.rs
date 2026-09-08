//! Tests for bounded iTerm2 image output packets.

use super::*;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_image::{DecodedImage, GraphicsError, GraphicsProtocol, ImageDimension};
use std::io;

const EXPECTED_RED_PNG_BASE64: &[u8] =
    b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAEElEQVR4AQEFAPr/AP8AAP8FAAH/+lyI0QAAAABJRU5ErkJggg==";
const OSC_PREFIX_LEN: usize = b"\x1b]1337;".len();
const OSC_ST_LEN: usize = 2;
const PACKET_LIMIT: usize = 64 * 1024;

fn red_image() -> DecodedImage {
    DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    }
}

fn independently_encoded_red_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");
    bytes
}

fn packet_body(packet: &[u8]) -> &[u8] {
    assert!(packet.starts_with(b"\x1b]1337;"));
    assert!(packet.ends_with(b"\x1b\\"));
    &packet[OSC_PREFIX_LEN..packet.len() - OSC_ST_LEN]
}

fn collect_packets(encoder: &mut Encoder) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    while let Some(packet) = encoder.next_packet() {
        packets.push(packet.to_vec());
    }
    packets
}

fn large_image() -> DecodedImage {
    let width = 256;
    let height = 256;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for index in 0..(width * height) {
        let value = index as u32;
        rgba.extend_from_slice(&[
            value as u8,
            (value >> 8) as u8,
            (value.wrapping_mul(37) >> 16) as u8,
            255,
        ]);
    }
    DecodedImage {
        width: width as u32,
        height: height as u32,
        rgba,
    }
}

#[test]
fn small_file_has_exact_wire_fixture_and_round_trip() {
    let image = red_image();
    let mut encoder = Encoder::new(&image, OutputOptions::new(4, 5).expect("dimensions"))
        .expect("valid image output");
    let packet = encoder.next_packet().expect("one file packet").to_vec();
    let expected_header =
        b"\x1b]1337;File=inline=1;width=4;height=5;preserveAspectRatio=0;size=73:";
    let expected = [
        expected_header.as_slice(),
        EXPECTED_RED_PNG_BASE64,
        b"\x1b\\".as_slice(),
    ]
    .concat();
    assert_eq!(packet, expected);
    assert!(encoder.next_packet().is_none());
    assert!(encoder.is_complete());

    let encoded = &packet[expected_header.len()..packet.len() - OSC_ST_LEN];
    assert_eq!(encoded, EXPECTED_RED_PNG_BASE64);
    let png = STANDARD
        .decode(encoded)
        .expect("wire image is padded base64");
    let independently_decoded = image::load_from_memory(&png)
        .expect("wire PNG is independently decodable")
        .into_rgba8();
    assert_eq!(independently_decoded.width(), 1);
    assert_eq!(independently_decoded.height(), 1);
    assert_eq!(independently_decoded.into_raw(), image.rgba);

    let mut state = None;
    let result = crate::parse_iterm_command(packet_body(&packet), &mut state)
        .expect("encoded command parses")
        .expect("encoded command completes");
    assert_eq!(result.image, image);
    assert_eq!(result.display.width, Some(ImageDimension::Cells(4)));
    assert_eq!(result.display.height, Some(ImageDimension::Cells(5)));
    assert!(!result.display.preserve_aspect_ratio);
    assert!(state.is_none());
}

#[test]
fn multipart_packets_have_exact_framing_and_independent_png_round_trip() {
    let image = large_image();
    let mut encoder = Encoder::new(&image, OutputOptions::new(2, 3).expect("dimensions"))
        .expect("valid image output");
    let packets = collect_packets(&mut encoder);
    assert!(packets.len() > 3);
    assert!(encoder.is_complete());
    assert!(packets.iter().all(|packet| packet.len() <= PACKET_LIMIT));

    let independently_encoded = {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut bytes,
            CompressionType::Fast,
            FilterType::NoFilter,
        )
        .write_image(
            &image.rgba,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )
        .expect("independent PNG encoding");
        bytes
    };
    let expected_header = format!(
        "\x1b]1337;MultipartFile=inline=1;width=2;height=3;preserveAspectRatio=0;size={}\x1b\\",
        independently_encoded.len()
    );
    assert_eq!(packets[0], expected_header.as_bytes());
    assert_eq!(
        packets.last().expect("file end"),
        b"\x1b]1337;FileEnd\x1b\\"
    );

    let mut joined_base64 = Vec::new();
    let mut parsed = None;
    let mut state = None;
    for (index, packet) in packets.iter().enumerate() {
        let body = packet_body(packet);
        if index == 0 {
            assert!(body.starts_with(b"MultipartFile="));
        } else if index + 1 == packets.len() {
            assert_eq!(body, b"FileEnd");
        } else {
            assert!(body.starts_with(b"FilePart="));
            let encoded = &body[b"FilePart=".len()..];
            assert!(!encoded.is_empty());
            assert_eq!(encoded.len() % 4, 0);
            joined_base64.extend_from_slice(encoded);
        }
        let result = crate::parse_iterm_command(body, &mut state).expect("each packet parses");
        if let Some(result) = result {
            assert!(parsed.is_none());
            parsed = Some(result);
        }
    }
    assert!(state.is_none());
    let parsed = parsed.expect("file end returns image");
    assert_eq!(parsed.image, image);
    assert_eq!(parsed.display.width, Some(ImageDimension::Cells(2)));
    assert_eq!(parsed.display.height, Some(ImageDimension::Cells(3)));
    assert!(!parsed.display.preserve_aspect_ratio);

    let png = STANDARD
        .decode(&joined_base64)
        .expect("joined multipart base64 is padded");
    let independently_decoded = image::load_from_memory(&png)
        .expect("multipart PNG is independently decodable")
        .into_rgba8();
    assert_eq!(independently_decoded.width(), image.width);
    assert_eq!(independently_decoded.height(), image.height);
    assert_eq!(independently_decoded.into_raw(), image.rgba);
}

#[test]
fn packet_reset_restarts_the_transfer() {
    let image = red_image();
    let mut encoder = Encoder::new(&image, OutputOptions::new(1, 1).expect("dimensions"))
        .expect("valid image output");
    let first = encoder.next_packet().expect("file packet").to_vec();
    assert!(encoder.is_complete());
    encoder.reset();
    assert!(!encoder.is_complete());
    assert_eq!(
        encoder.next_packet().expect("file packet after reset"),
        first
    );
    assert!(encoder.is_complete());
}

#[test]
fn encoder_rejects_zero_and_bad_rgba_before_write() {
    assert_eq!(
        OutputOptions::new(0, 1),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    assert_eq!(
        OutputOptions::new(1, 0),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    let mut mutated_options = OutputOptions::new(1, 1).expect("valid options");
    mutated_options.width_cells = 0;
    let image = red_image();
    let error = Encoder::new(&image, mutated_options).expect_err("mutated dimensions");
    assert_eq!(
        error_graphics(error),
        GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        }
    );

    let options = OutputOptions::new(1, 1).expect("valid options");
    let bad = DecodedImage {
        width: 2,
        height: 1,
        rgba: vec![0, 0, 0, 255],
    };
    let error = Encoder::new(&bad, options).expect_err("bad RGBA length");
    assert_eq!(
        error_graphics(error),
        GraphicsError::DeclaredSizeMismatch {
            protocol: GraphicsProtocol::Iterm2,
            expected: 8,
            actual: 4,
        }
    );
}

#[test]
fn padded_base64_limit_has_exact_raw_boundaries() {
    assert_eq!(
        base64_encoded_len(MAX_PNG_BYTES).expect("maximum padded payload fits"),
        MAX_GRAPHICS_TRANSFER_BYTES
    );
    let error = base64_encoded_len(MAX_PNG_BYTES + 1).expect_err("one extra raw byte is too large");
    let EncodeError::Graphics(GraphicsError::TransferTooLarge { protocol }) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(protocol, GraphicsProtocol::Iterm2);
}

#[test]
fn encoder_rejects_overflow_dimensions_before_png_allocation() {
    let image = DecodedImage {
        width: u32::MAX,
        height: u32::MAX,
        rgba: Vec::new(),
    };
    let options = OutputOptions::new(1, 1).expect("valid options");
    let error = Encoder::new(&image, options).expect_err("dimensions exceed limits");
    assert_eq!(
        error_graphics(error),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Iterm2
        }
    );
}

fn error_graphics(error: EncodeError) -> GraphicsError {
    match error {
        EncodeError::Graphics(error) => error,
        other => panic!("unexpected error: {other:?}"),
    }
}

struct PartialFailingWriter {
    bytes: Vec<u8>,
    accepted: usize,
}

impl io::Write for PartialFailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let accepted = bytes.len().min(self.accepted);
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn writer_error_is_preserved_without_advancing_packet_state() {
    let image = red_image();
    let mut encoder =
        Encoder::new(&image, OutputOptions::new(1, 1).expect("dimensions")).expect("valid image");
    let mut writer = PartialFailingWriter {
        bytes: Vec::new(),
        accepted: 5,
    };
    let error = encoder
        .write_next_packet(&mut writer)
        .expect_err("writer fails");
    match error {
        EncodeError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::BrokenPipe),
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(writer.bytes.len(), 5);
    assert!(!encoder.is_complete());
    let packet_after_error = encoder.next_packet().expect("same packet remains");
    assert_eq!(&packet_after_error[..5], writer.bytes.as_slice());
    assert!(encoder.is_complete());
}

#[test]
fn successful_writer_emits_one_packet_and_reports_completion() {
    let image = red_image();
    let mut encoder =
        Encoder::new(&image, OutputOptions::new(1, 1).expect("dimensions")).expect("valid image");
    let mut output = Vec::new();
    assert!(encoder
        .write_next_packet(&mut output)
        .expect("writer accepts packet"));
    assert!(!encoder
        .write_next_packet(&mut output)
        .expect("completed encoder has no packet"));
    assert!(encoder.is_complete());
    assert_eq!(output, independently_encoded_red_png_wire());
}

fn independently_encoded_red_png_wire() -> Vec<u8> {
    let png = independently_encoded_red_png();
    let encoded = STANDARD.encode(png);
    format!(
        "\x1b]1337;File=inline=1;width=1;height=1;preserveAspectRatio=0;size={}:{}\x1b\\",
        73, encoded
    )
    .into_bytes()
}
