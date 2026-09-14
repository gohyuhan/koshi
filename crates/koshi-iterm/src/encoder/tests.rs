//! Tests for bounded iTerm2 image output packets.

use super::*;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_image::{DecodedImage, GraphicsError, GraphicsProtocol, ImageDimension};
use std::io;

const EXPECTED_RED_PNG_BASE64: &[u8] =
    b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAEElEQVR4AQEFAPr/AP8AAP8FAAH/+lyI0QAAAABJRU5ErkJggg==";
const ITERM_OSC_PREFIX_BYTE_COUNT: usize = b"\x1b]1337;".len();
const ITERM_OSC_TERMINATOR_BYTE_COUNT: usize = 2;
const MAX_PACKET_BYTE_COUNT: usize = 64 * 1024;

fn build_red_image() -> DecodedImage {
    DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    }
}

fn build_independently_encoded_red_png() -> Vec<u8> {
    let mut encoded_png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded_png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");
    encoded_png_bytes
}

fn extract_packet_body(packet_bytes: &[u8]) -> &[u8] {
    assert!(packet_bytes.starts_with(b"\x1b]1337;"));
    assert!(packet_bytes.ends_with(b"\x1b\\"));
    &packet_bytes[ITERM_OSC_PREFIX_BYTE_COUNT..packet_bytes.len() - ITERM_OSC_TERMINATOR_BYTE_COUNT]
}

fn collect_iterm_packets(iterm_encoder: &mut ItermEncoder) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    while let Some(packet_bytes) = iterm_encoder.take_next_packet() {
        packets.push(packet_bytes.to_vec());
    }
    packets
}

fn build_large_image() -> DecodedImage {
    let pixel_width = 256;
    let pixel_height = 256;
    let mut rgba_bytes = Vec::with_capacity(pixel_width * pixel_height * 4);
    for pixel_index in 0..(pixel_width * pixel_height) {
        let pixel_value = pixel_index as u32;
        rgba_bytes.extend_from_slice(&[
            pixel_value as u8,
            (pixel_value >> 8) as u8,
            (pixel_value.wrapping_mul(37) >> 16) as u8,
            255,
        ]);
    }
    DecodedImage {
        pixel_width: pixel_width as u32,
        pixel_height: pixel_height as u32,
        rgba_bytes,
    }
}

#[test]
fn small_file_has_exact_wire_fixture_and_round_trip() {
    let image = build_red_image();
    let mut iterm_encoder = ItermEncoder::from_image(
        &image,
        ItermOutputOptions::from_cell_dimensions(4, 5).expect("dimensions"),
    )
    .expect("valid image output");
    let packet_bytes = iterm_encoder
        .take_next_packet()
        .expect("one file packet")
        .to_vec();
    let expected_header =
        b"\x1b]1337;File=inline=1;width=4;height=5;preserveAspectRatio=0;size=73:";
    let expected_packet_bytes = [
        expected_header.as_slice(),
        EXPECTED_RED_PNG_BASE64,
        b"\x1b\\".as_slice(),
    ]
    .concat();
    assert_eq!(packet_bytes, expected_packet_bytes);
    assert!(iterm_encoder.take_next_packet().is_none());
    assert!(iterm_encoder.is_complete());

    let encoded_png_base64 =
        &packet_bytes[expected_header.len()..packet_bytes.len() - ITERM_OSC_TERMINATOR_BYTE_COUNT];
    assert_eq!(encoded_png_base64, EXPECTED_RED_PNG_BASE64);
    let png_bytes = STANDARD
        .decode(encoded_png_base64)
        .expect("wire image is padded base64");
    let independently_decoded_image = image::load_from_memory(&png_bytes)
        .expect("wire PNG is independently decodable")
        .into_rgba8();
    assert_eq!(independently_decoded_image.width(), 1);
    assert_eq!(independently_decoded_image.height(), 1);
    assert_eq!(independently_decoded_image.into_raw(), image.rgba_bytes);

    let mut multipart_transfer = None;
    let decoded_graphics =
        crate::parse_iterm_command(extract_packet_body(&packet_bytes), &mut multipart_transfer)
            .expect("encoded command parses")
            .expect("encoded command completes");
    assert_eq!(decoded_graphics.image, image);
    assert_eq!(
        decoded_graphics.display.requested_width,
        Some(ImageDimension::Cells(4))
    );
    assert_eq!(
        decoded_graphics.display.requested_height,
        Some(ImageDimension::Cells(5))
    );
    assert!(!decoded_graphics.display.is_aspect_ratio_preserved);
    assert!(multipart_transfer.is_none());
}

#[test]
fn multipart_packets_have_exact_framing_and_independent_png_round_trip() {
    let image = build_large_image();
    let mut iterm_encoder = ItermEncoder::from_image(
        &image,
        ItermOutputOptions::from_cell_dimensions(2, 3).expect("dimensions"),
    )
    .expect("valid image output");
    let encoded_packets = collect_iterm_packets(&mut iterm_encoder);
    assert!(encoded_packets.len() > 3);
    assert!(iterm_encoder.is_complete());
    assert!(encoded_packets
        .iter()
        .all(|encoded_packet_bytes| encoded_packet_bytes.len() <= MAX_PACKET_BYTE_COUNT));

    let independently_encoded_png_bytes = {
        let mut independently_encoded_png_bytes = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut independently_encoded_png_bytes,
            CompressionType::Fast,
            FilterType::NoFilter,
        )
        .write_image(
            &image.rgba_bytes,
            image.pixel_width,
            image.pixel_height,
            ExtendedColorType::Rgba8,
        )
        .expect("independent PNG encoding");
        independently_encoded_png_bytes
    };
    let expected_header = format!(
        "\x1b]1337;MultipartFile=inline=1;width=2;height=3;preserveAspectRatio=0;size={}\x1b\\",
        independently_encoded_png_bytes.len()
    );
    assert_eq!(encoded_packets[0], expected_header.as_bytes());
    assert_eq!(
        encoded_packets.last().expect("file end"),
        b"\x1b]1337;FileEnd\x1b\\"
    );

    let mut joined_base64 = Vec::new();
    let mut parsed_graphics = None;
    let mut multipart_transfer = None;
    for (packet_index, encoded_packet_bytes) in encoded_packets.iter().enumerate() {
        let command_body = extract_packet_body(encoded_packet_bytes);
        if packet_index == 0 {
            assert!(command_body.starts_with(b"MultipartFile="));
        } else if packet_index + 1 == encoded_packets.len() {
            assert_eq!(command_body, b"FileEnd");
        } else {
            assert!(command_body.starts_with(b"FilePart="));
            let encoded_part_base64 = &command_body[b"FilePart=".len()..];
            assert!(!encoded_part_base64.is_empty());
            assert_eq!(encoded_part_base64.len() % 4, 0);
            joined_base64.extend_from_slice(encoded_part_base64);
        }
        let decoded_graphics_option =
            crate::parse_iterm_command(command_body, &mut multipart_transfer)
                .expect("each packet parses");
        if let Some(decoded_graphics) = decoded_graphics_option {
            assert!(parsed_graphics.is_none());
            parsed_graphics = Some(decoded_graphics);
        }
    }
    assert!(multipart_transfer.is_none());
    let parsed_graphics = parsed_graphics.expect("file end returns image");
    assert_eq!(parsed_graphics.image, image);
    assert_eq!(
        parsed_graphics.display.requested_width,
        Some(ImageDimension::Cells(2))
    );
    assert_eq!(
        parsed_graphics.display.requested_height,
        Some(ImageDimension::Cells(3))
    );
    assert!(!parsed_graphics.display.is_aspect_ratio_preserved);

    let multipart_png_bytes = STANDARD
        .decode(&joined_base64)
        .expect("joined multipart base64 is padded");
    let independently_decoded_image = image::load_from_memory(&multipart_png_bytes)
        .expect("multipart PNG is independently decodable")
        .into_rgba8();
    assert_eq!(independently_decoded_image.width(), image.pixel_width);
    assert_eq!(independently_decoded_image.height(), image.pixel_height);
    assert_eq!(independently_decoded_image.into_raw(), image.rgba_bytes);
}

#[test]
fn packet_reset_restarts_the_transfer() {
    let image = build_red_image();
    let mut iterm_encoder = ItermEncoder::from_image(
        &image,
        ItermOutputOptions::from_cell_dimensions(1, 1).expect("dimensions"),
    )
    .expect("valid image output");
    let first_packet_bytes = iterm_encoder
        .take_next_packet()
        .expect("file packet")
        .to_vec();
    assert!(iterm_encoder.is_complete());
    iterm_encoder.reset_packet_emission();
    assert!(!iterm_encoder.is_complete());
    assert_eq!(
        iterm_encoder
            .take_next_packet()
            .expect("file packet after reset"),
        first_packet_bytes
    );
    assert!(iterm_encoder.is_complete());
}

#[test]
fn encoder_rejects_zero_and_bad_rgba_before_write() {
    assert_eq!(
        ItermOutputOptions::from_cell_dimensions(0, 1),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    assert_eq!(
        ItermOutputOptions::from_cell_dimensions(1, 0),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        })
    );
    let mut mutated_output_options =
        ItermOutputOptions::from_cell_dimensions(1, 1).expect("valid options");
    mutated_output_options.width_cells = 0;
    let image = build_red_image();
    let encode_error =
        ItermEncoder::from_image(&image, mutated_output_options).expect_err("mutated dimensions");
    assert_eq!(
        extract_graphics_error(encode_error),
        GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2
        }
    );

    let output_options = ItermOutputOptions::from_cell_dimensions(1, 1).expect("valid options");
    let malformed_image = DecodedImage {
        pixel_width: 2,
        pixel_height: 1,
        rgba_bytes: vec![0, 0, 0, 255],
    };
    let encode_error =
        ItermEncoder::from_image(&malformed_image, output_options).expect_err("bad RGBA length");
    assert_eq!(
        extract_graphics_error(encode_error),
        GraphicsError::DeclaredSizeMismatch {
            protocol: GraphicsProtocol::Iterm2,
            expected_byte_count: 8,
            actual_byte_count: 4,
        }
    );
}

#[test]
fn padded_base64_limit_has_exact_raw_boundaries() {
    assert_eq!(
        compute_base64_encoded_byte_count(MAX_ITERM_PNG_BYTE_COUNT)
            .expect("maximum padded payload fits"),
        MAX_GRAPHICS_TRANSFER_BYTE_COUNT
    );
    let encode_error = compute_base64_encoded_byte_count(MAX_ITERM_PNG_BYTE_COUNT + 1)
        .expect_err("one extra raw byte is too large");
    let ItermEncodeError::Graphics(GraphicsError::TransferTooLarge { protocol }) = encode_error
    else {
        panic!("unexpected error: {encode_error:?}");
    };
    assert_eq!(protocol, GraphicsProtocol::Iterm2);
}

#[test]
fn encoder_rejects_overflow_dimensions_before_png_allocation() {
    let image = DecodedImage {
        pixel_width: u32::MAX,
        pixel_height: u32::MAX,
        rgba_bytes: Vec::new(),
    };
    let output_options = ItermOutputOptions::from_cell_dimensions(1, 1).expect("valid options");
    let encode_error =
        ItermEncoder::from_image(&image, output_options).expect_err("dimensions exceed limits");
    assert_eq!(
        extract_graphics_error(encode_error),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Iterm2
        }
    );
}

fn extract_graphics_error(encode_error: ItermEncodeError) -> GraphicsError {
    match encode_error {
        ItermEncodeError::Graphics(graphics_error) => graphics_error,
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

struct PartialFailingWriter {
    written_bytes: Vec<u8>,
    accepted_byte_count: usize,
}

impl io::Write for PartialFailingWriter {
    fn write(&mut self, requested_bytes: &[u8]) -> io::Result<usize> {
        let accepted_byte_count = requested_bytes.len().min(self.accepted_byte_count);
        self.written_bytes
            .extend_from_slice(&requested_bytes[..accepted_byte_count]);
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn writer_error_is_preserved_without_advancing_packet_state() {
    let image = build_red_image();
    let mut iterm_encoder = ItermEncoder::from_image(
        &image,
        ItermOutputOptions::from_cell_dimensions(1, 1).expect("dimensions"),
    )
    .expect("valid image");
    let mut writer = PartialFailingWriter {
        written_bytes: Vec::new(),
        accepted_byte_count: 5,
    };
    let encode_error = iterm_encoder
        .write_next_packet(&mut writer)
        .expect_err("writer fails");
    match encode_error {
        ItermEncodeError::Io(io_error) => {
            assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe)
        }
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
    assert_eq!(writer.written_bytes.len(), 5);
    assert!(!iterm_encoder.is_complete());
    let packet_after_error = iterm_encoder
        .take_next_packet()
        .expect("same packet remains");
    assert_eq!(&packet_after_error[..5], writer.written_bytes.as_slice());
    assert!(iterm_encoder.is_complete());
}

#[test]
fn successful_writer_emits_one_packet_and_reports_completion() {
    let image = build_red_image();
    let mut iterm_encoder = ItermEncoder::from_image(
        &image,
        ItermOutputOptions::from_cell_dimensions(1, 1).expect("dimensions"),
    )
    .expect("valid image");
    let mut output_bytes = Vec::new();
    assert!(iterm_encoder
        .write_next_packet(&mut output_bytes)
        .expect("writer accepts packet"));
    assert!(!iterm_encoder
        .write_next_packet(&mut output_bytes)
        .expect("completed encoder has no packet"));
    assert!(iterm_encoder.is_complete());
    assert_eq!(output_bytes, independently_encoded_red_png_wire());
}

fn independently_encoded_red_png_wire() -> Vec<u8> {
    let independently_encoded_png_bytes = build_independently_encoded_red_png();
    let encoded_png_base64 = STANDARD.encode(independently_encoded_png_bytes);
    format!(
        "\x1b]1337;File=inline=1;width=1;height=1;preserveAspectRatio=0;size={}:{}\x1b\\",
        73, encoded_png_base64
    )
    .into_bytes()
}
