//! Tests for Kitty image encoding and protocol output.

use super::*;
use std::io::{self, Read};

use koshi_image::DecodedImage;

fn build_decoded_image(
    image_width_pixels: u32,
    image_height_pixels: u32,
    rgba_bytes: Vec<u8>,
) -> DecodedImage {
    DecodedImage {
        pixel_width: image_width_pixels,
        pixel_height: image_height_pixels,
        rgba_bytes,
    }
}

#[derive(Default)]
struct WriterFailureAfterByteLimit {
    kitty_output_bytes: Vec<u8>,
    maximum_output_byte_count: usize,
}

impl io::Write for WriterFailureAfterByteLimit {
    fn write(&mut self, kitty_output_bytes: &[u8]) -> io::Result<usize> {
        if self.kitty_output_bytes.len() >= self.maximum_output_byte_count {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "write limit"));
        }
        let remaining_byte_count = self.maximum_output_byte_count - self.kitty_output_bytes.len();
        let written_byte_count = remaining_byte_count.min(kitty_output_bytes.len());
        self.kitty_output_bytes
            .extend_from_slice(&kitty_output_bytes[..written_byte_count]);
        if written_byte_count < kitty_output_bytes.len() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "write limit"));
        }
        Ok(written_byte_count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build_pseudorandom_rgba_bytes(byte_count: usize) -> Vec<u8> {
    let mut pseudorandom_state = 0x1234_5678u32;
    (0..byte_count)
        .map(|_| {
            pseudorandom_state ^= pseudorandom_state << 13;
            pseudorandom_state ^= pseudorandom_state >> 17;
            pseudorandom_state ^= pseudorandom_state << 5;
            pseudorandom_state as u8
        })
        .collect()
}

#[test]
fn one_pixel_transmit_writes_the_exact_kitty_packet() {
    let compressed_bytes =
        compress_kitty_image(&build_decoded_image(1, 1, vec![1, 2, 3, 4]), || false)
            .expect("the image compresses")
            .expect("nothing cancels the compression");
    let mut kitty_output_bytes = Vec::new();

    write_kitty_transmit(&mut kitty_output_bytes, 1, 1, 7, &compressed_bytes)
        .expect("the transmit writes");

    assert_eq!(
        kitty_output_bytes,
        b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2,o=z,m=0;eAFjZGJmAQAAGAAL\x1b\\"
    );
}

#[test]
fn a_compressed_image_inflates_back_to_its_rgba_bytes() {
    // 2 MiB of RGBA takes eight compression steps; the stream inflates back
    // to the same bytes.
    let rgba_bytes = build_pseudorandom_rgba_bytes(2_097_152);
    let compressed_bytes =
        compress_kitty_image(&build_decoded_image(16_384, 32, rgba_bytes.clone()), || {
            false
        })
        .expect("the image compresses")
        .expect("nothing cancels the compression");
    let mut inflated_bytes = Vec::new();

    flate2::read::ZlibDecoder::new(compressed_bytes.as_slice())
        .read_to_end(&mut inflated_bytes)
        .expect("the stream inflates");

    assert_eq!(inflated_bytes, rgba_bytes);
}

#[test]
fn a_compression_stops_before_the_step_after_its_cancellation() {
    // The image needs eight steps. The cancellation answers `true` from its
    // third call, so two steps run and no stream comes back.
    let decoded_image = build_decoded_image(16_384, 32, vec![0; 2_097_152]);
    let cancellation_call_count = std::cell::Cell::new(0);

    let compression_result = compress_kitty_image(&decoded_image, || {
        cancellation_call_count.set(cancellation_call_count.get() + 1);
        cancellation_call_count.get() >= 3
    });

    assert!(matches!(compression_result, Ok(None)));
    assert_eq!(cancellation_call_count.get(), 3);
}

#[test]
fn compression_rejects_invalid_dimensions_and_rgba_length() {
    assert!(matches!(
        compress_kitty_image(&build_decoded_image(0, 1, Vec::new()), || false),
        Err(KittyOutputError::InvalidImageDimensions {
            image_width_pixels: 0,
            image_height_pixels: 1,
        })
    ));
    assert!(matches!(
        compress_kitty_image(&build_decoded_image(16_385, 1, Vec::new()), || false),
        Err(KittyOutputError::InvalidImageDimensions {
            image_width_pixels: 16_385,
            image_height_pixels: 1,
        })
    ));
    assert!(matches!(
        compress_kitty_image(&build_decoded_image(1, 1, vec![1]), || false),
        Err(KittyOutputError::InvalidImageRgbaByteCount {
            image_width_pixels: 1,
            image_height_pixels: 1,
            expected_rgba_byte_count: 4,
            actual_rgba_byte_count: 1,
        })
    ));
}

#[test]
fn a_transmit_cuts_its_payload_into_3072_byte_chunks() {
    // 6 145 payload bytes give three commands: two of 3 072 bytes that say
    // more follows, and one of 1 byte that says it is the last.
    let compressed_bytes = vec![0x5a; 6_145];
    let mut kitty_output_bytes = Vec::new();

    write_kitty_transmit(&mut kitty_output_bytes, 4, 3, 9, &compressed_bytes)
        .expect("the transmit writes");

    let full_chunk_base64 = STANDARD.encode(&compressed_bytes[..3_072]);
    let last_chunk_base64 = STANDARD.encode(&compressed_bytes[6_144..]);
    assert_eq!(
        String::from_utf8(kitty_output_bytes).expect("the transmit is text"),
        format!(
            "\x1b_Ga=t,f=32,s=4,v=3,i=9,q=2,o=z,m=1;{full_chunk_base64}\x1b\\\
             \x1b_Gq=2,m=1;{full_chunk_base64}\x1b\\\
             \x1b_Gq=2,m=0;{last_chunk_base64}\x1b\\"
        )
    );
}

#[test]
fn a_transmit_rejects_a_zero_image_id_and_writes_nothing() {
    let mut kitty_output_bytes = Vec::new();

    assert!(matches!(
        write_kitty_transmit(&mut kitty_output_bytes, 1, 1, 0, &[1, 2, 3]),
        Err(KittyOutputError::InvalidImageId)
    ));
    assert_eq!(kitty_output_bytes, Vec::<u8>::new());
}

#[test]
fn a_transmit_of_an_empty_payload_writes_nothing() {
    let mut kitty_output_bytes = Vec::new();

    write_kitty_transmit(&mut kitty_output_bytes, 1, 1, 7, &[]).expect("the transmit writes");

    assert_eq!(kitty_output_bytes, Vec::<u8>::new());
}

#[test]
fn a_transmit_reports_a_writer_failure() {
    let mut writer = WriterFailureAfterByteLimit {
        kitty_output_bytes: Vec::new(),
        maximum_output_byte_count: 8,
    };

    let kitty_output_error = write_kitty_transmit(&mut writer, 1, 1, 7, &[1, 2, 3])
        .expect_err("the write past 8 bytes fails");

    assert!(matches!(
        kitty_output_error,
        KittyOutputError::Io(ref io_error) if io_error.kind() == io::ErrorKind::BrokenPipe
    ));
    assert_eq!(writer.kitty_output_bytes, b"\x1b_Ga=t,f");
}

#[test]
fn placement_writes_source_offsets_and_z_index() {
    let decoded_image = DecodedImage {
        pixel_width: 4,
        pixel_height: 3,
        rgba_bytes: vec![0; 4 * 3 * 4],
    };
    let placement = KittyPlacement {
        image_id: 7,
        placement_id: 9,
        source_x_pixels: 1,
        source_y_pixels: 2,
        source_width_pixels: 2,
        source_height_pixels: 1,
        column_count: 3,
        row_count: 4,
        cell_pixel_offset_x: Some(5),
        cell_pixel_offset_y: Some(6),
        z_index: -2,
    };
    let mut kitty_output_bytes = Vec::new();

    write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement)
        .expect("the placement writes");

    assert_eq!(
        kitty_output_bytes,
        b"\x1b_Ga=p,i=7,p=9,x=1,y=2,w=2,h=1,X=5,Y=6,c=3,r=4,C=1,z=-2,q=2;\x1b\\"
    );
}

#[test]
fn placement_rejects_zero_ids_dimensions_and_out_of_bounds_source() {
    let decoded_image = DecodedImage {
        pixel_width: 2,
        pixel_height: 2,
        rgba_bytes: vec![0; 16],
    };
    let valid_placement = KittyPlacement {
        image_id: 1,
        placement_id: 1,
        source_x_pixels: 0,
        source_y_pixels: 0,
        source_width_pixels: 1,
        source_height_pixels: 1,
        column_count: 1,
        row_count: 1,
        cell_pixel_offset_x: None,
        cell_pixel_offset_y: None,
        z_index: 0,
    };
    let mut kitty_output_bytes = Vec::new();

    let mut placement = valid_placement;
    placement.image_id = 0;
    assert!(matches!(
        write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement),
        Err(KittyOutputError::InvalidImageId)
    ));
    placement = valid_placement;
    placement.placement_id = 0;
    assert!(matches!(
        write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement),
        Err(KittyOutputError::InvalidPlacementId)
    ));
    placement = valid_placement;
    placement.column_count = 0;
    assert!(matches!(
        write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement),
        Err(KittyOutputError::InvalidPlacementDimensions {
            column_count: 0,
            row_count: 1,
        })
    ));
    placement = valid_placement;
    placement.source_x_pixels = 2;
    assert!(matches!(
        write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement),
        Err(KittyOutputError::InvalidSourceRect {
            source_x_pixels: 2,
            source_y_pixels: 0,
            source_width_pixels: 1,
            source_height_pixels: 1,
            image_width_pixels: 2,
            image_height_pixels: 2,
        })
    ));
}

#[test]
fn delete_commands_write_exact_kitty_bytes() {
    let mut kitty_output_bytes = Vec::new();

    write_kitty_image_delete(&mut kitty_output_bytes, 7).expect("the image delete writes");
    write_kitty_delete_all(&mut kitty_output_bytes).expect("the all delete writes");
    write_kitty_visible_placement_delete(&mut kitty_output_bytes)
        .expect("the placement-only delete writes");

    assert_eq!(
        kitty_output_bytes,
        b"\x1b_Ga=d,d=I,i=7,q=2;\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\"
    );
}

#[test]
fn abort_writes_the_exact_open_transfer_cancellation_bytes() {
    let mut kitty_output_bytes = Vec::new();

    write_kitty_abort(&mut kitty_output_bytes).expect("the abort writes");

    assert_eq!(kitty_output_bytes, b"\x18\x1b\\");
}

#[test]
fn image_delete_rejects_a_zero_image_id() {
    let mut kitty_output_bytes = Vec::new();

    assert!(matches!(
        write_kitty_image_delete(&mut kitty_output_bytes, 0),
        Err(KittyOutputError::InvalidImageId)
    ));
    assert!(kitty_output_bytes.is_empty());
}
