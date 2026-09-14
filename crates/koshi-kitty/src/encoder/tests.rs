//! Tests for Kitty image encoding and protocol output.

use super::*;
use std::io;
use std::sync::Arc;

use flate2::{Compress, Compression};
use koshi_image::DecodedImage;

fn build_decoded_image(
    image_width_pixels: u32,
    image_height_pixels: u32,
    rgba_bytes: Vec<u8>,
) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        pixel_width: image_width_pixels,
        pixel_height: image_height_pixels,
        rgba_bytes,
    })
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
        let remaining = self.maximum_output_byte_count - self.kitty_output_bytes.len();
        let written_byte_count = remaining.min(kitty_output_bytes.len());
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
fn one_pixel_upload_writes_the_exact_kitty_packet() {
    let mut upload =
        KittyUpload::from_decoded_image(build_decoded_image(1, 1, vec![1, 2, 3, 4]), 7)
            .expect("the image is valid");
    let mut kitty_output_bytes = Vec::new();

    upload
        .advance_upload(&mut kitty_output_bytes)
        .expect("the upload writes");

    assert_eq!(
        kitty_output_bytes,
        b"\x1b_Ga=t,f=32,s=1,v=1,I=7,q=2,o=z,m=0;eAFjZGJmAQAAGAAL\x1b\\"
    );
    assert!(upload.has_started_transmission());
    assert!(upload.is_upload_complete());
}

#[test]
fn upload_retains_the_caller_arc_and_exposes_its_identity() {
    let decoded_image = build_decoded_image(1, 1, vec![1, 2, 3, 4]);
    let upload = KittyUpload::from_decoded_image(Arc::clone(&decoded_image), 11)
        .expect("the image is valid");

    assert!(Arc::ptr_eq(upload.get_decoded_image(), &decoded_image));
    assert_eq!(upload.get_image_number(), 11);
    assert!(!upload.has_started_transmission());
    assert!(!upload.is_upload_complete());
}

#[test]
fn upload_rejects_zero_image_number() {
    assert!(matches!(
        KittyUpload::from_decoded_image(build_decoded_image(1, 1, vec![1, 2, 3, 4]), 0),
        Err(KittyOutputError::InvalidImageNumber)
    ));
}

#[test]
fn upload_rejects_invalid_dimensions_and_rgba_length() {
    assert!(matches!(
        KittyUpload::from_decoded_image(build_decoded_image(0, 1, Vec::new()), 1),
        Err(KittyOutputError::InvalidImageDimensions {
            image_width_pixels: 0,
            image_height_pixels: 1,
        })
    ));
    assert!(matches!(
        KittyUpload::from_decoded_image(build_decoded_image(16_385, 1, Vec::new()), 1),
        Err(KittyOutputError::InvalidImageDimensions {
            image_width_pixels: 16_385,
            image_height_pixels: 1,
        })
    ));
    assert!(matches!(
        KittyUpload::from_decoded_image(build_decoded_image(1, 1, vec![1]), 1),
        Err(KittyOutputError::InvalidImageRgbaByteCount {
            image_width_pixels: 1,
            image_height_pixels: 1,
            expected_rgba_byte_count: 4,
            actual_rgba_byte_count: 1,
        })
    ));
}

#[test]
fn upload_reports_a_writer_failure_at_a_packet_boundary_without_progress() {
    let rgba_bytes = build_pseudorandom_rgba_bytes(16_384);
    let mut expected_upload =
        KittyUpload::from_decoded_image(build_decoded_image(4_096, 1, rgba_bytes.clone()), 3)
            .expect("the image is valid");
    let mut expected_kitty_output_bytes = Vec::new();
    expected_upload
        .advance_upload(&mut expected_kitty_output_bytes)
        .expect("the complete output step writes");
    let first_packet_end_byte_offset = expected_kitty_output_bytes
        .windows(2)
        .position(|packet_bytes| packet_bytes == b"\x1b\\")
        .expect("the output has a packet terminator")
        + 2;

    let mut upload = KittyUpload::from_decoded_image(build_decoded_image(4_096, 1, rgba_bytes), 3)
        .expect("the image is valid");
    let mut writer = WriterFailureAfterByteLimit {
        kitty_output_bytes: Vec::new(),
        maximum_output_byte_count: first_packet_end_byte_offset,
    };
    let kitty_output_error = upload
        .advance_upload(&mut writer)
        .expect_err("the second packet write fails");

    assert!(matches!(
        kitty_output_error,
        KittyOutputError::Io(ref io_error) if io_error.kind() == io::ErrorKind::BrokenPipe
    ));
    assert_eq!(
        writer.kitty_output_bytes,
        expected_kitty_output_bytes[..first_packet_end_byte_offset]
    );
    assert!(!upload.has_started_transmission());
    assert!(!upload.is_upload_complete());
}

#[test]
fn one_upload_advance_compresses_at_most_256_kibibytes_of_input() {
    let mut upload =
        KittyUpload::from_decoded_image(build_decoded_image(16_384, 32, vec![0; 2_097_152]), 1)
            .expect("the image is valid");
    let mut kitty_output_bytes = Vec::new();

    upload
        .advance_upload(&mut kitty_output_bytes)
        .expect("the bounded step writes");

    assert_eq!(
        upload.input_byte_offset,
        KITTY_COMPRESSION_INPUT_BYTE_COUNT_PER_STEP
    );
    assert!(!upload.is_compression_complete);
}

#[test]
fn one_upload_advance_writes_at_most_sixteen_compressed_chunks() {
    let decoded_image = build_decoded_image(1, 1, vec![1, 2, 3, 4]);
    let mut upload = KittyUpload {
        decoded_image,
        image_number: 7,
        compressor: Compress::new(Compression::fast(), true),
        input_byte_offset: 4,
        compressed_bytes: vec![
            0x5a;
            KITTY_IMAGE_CHUNK_BYTE_COUNT * (KITTY_IMAGE_CHUNK_COUNT_PER_STEP + 1)
        ],
        compressed_byte_offset: 0,
        is_compression_complete: true,
        has_started_transmission: false,
    };
    let mut kitty_output_bytes = Vec::new();

    upload
        .advance_upload(&mut kitty_output_bytes)
        .expect("the bounded step writes");

    assert_eq!(
        upload.compressed_byte_offset,
        KITTY_IMAGE_CHUNK_BYTE_COUNT * KITTY_IMAGE_CHUNK_COUNT_PER_STEP
    );
    assert!(upload.has_started_transmission());
    assert!(!upload.is_upload_complete());
    assert_eq!(
        kitty_output_bytes
            .windows(b"\x1b_G".len())
            .filter(|packet_bytes| *packet_bytes == b"\x1b_G")
            .count(),
        KITTY_IMAGE_CHUNK_COUNT_PER_STEP
    );
    assert!(kitty_output_bytes
        .windows(4)
        .all(|packet_bytes| packet_bytes != b"m=0;"));
}

#[test]
fn placement_writes_source_offsets_and_z_index() {
    let decoded_image = DecodedImage {
        pixel_width: 4,
        pixel_height: 3,
        rgba_bytes: vec![0; 4 * 3 * 4],
    };
    let placement = KittyPlacement {
        image_number: 7,
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
        b"\x1b_Ga=p,I=7,p=9,x=1,y=2,w=2,h=1,X=5,Y=6,c=3,r=4,C=1,z=-2,q=2;\x1b\\"
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
        image_number: 1,
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
    placement.image_number = 0;
    assert!(matches!(
        write_kitty_placement(&mut kitty_output_bytes, &decoded_image, &placement),
        Err(KittyOutputError::InvalidImageNumber)
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
    write_kitty_placement_delete(&mut kitty_output_bytes, 7, 9)
        .expect("the placement delete writes");
    write_kitty_delete_all(&mut kitty_output_bytes).expect("the all delete writes");
    write_kitty_visible_placement_delete(&mut kitty_output_bytes)
        .expect("the placement-only delete writes");

    assert_eq!(
        kitty_output_bytes,
        b"\x1b_Ga=d,d=N,I=7,q=2;\x1b\\\x1b_Ga=d,d=n,I=7,p=9,q=2;\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\"
    );
}

#[test]
fn abort_writes_the_exact_open_transfer_cancellation_bytes() {
    let mut kitty_output_bytes = Vec::new();

    write_kitty_abort(&mut kitty_output_bytes).expect("the abort writes");

    assert_eq!(kitty_output_bytes, b"\x18\x1b\\");
}

#[test]
fn delete_commands_reject_zero_ids() {
    let mut kitty_output_bytes = Vec::new();

    assert!(matches!(
        write_kitty_image_delete(&mut kitty_output_bytes, 0),
        Err(KittyOutputError::InvalidImageNumber)
    ));
    assert!(matches!(
        write_kitty_placement_delete(&mut kitty_output_bytes, 0, 1),
        Err(KittyOutputError::InvalidImageNumber)
    ));
    assert!(matches!(
        write_kitty_placement_delete(&mut kitty_output_bytes, 1, 0),
        Err(KittyOutputError::InvalidPlacementId)
    ));
    assert!(kitty_output_bytes.is_empty());
}
