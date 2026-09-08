//! Tests for Kitty image encoding and protocol output.

use super::*;
use std::io;
use std::sync::Arc;

use flate2::{Compress, Compression};
use koshi_image::DecodedImage;

fn image(width: u32, height: u32, rgba: Vec<u8>) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        width,
        height,
        rgba,
    })
}

#[derive(Default)]
struct FailAfter {
    output: Vec<u8>,
    limit: usize,
}

impl io::Write for FailAfter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.output.len() >= self.limit {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "write limit"));
        }
        let remaining = self.limit - self.output.len();
        let count = remaining.min(bytes.len());
        self.output.extend_from_slice(&bytes[..count]);
        if count < bytes.len() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "write limit"));
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn random_rgba(length: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

#[test]
fn one_pixel_upload_writes_the_exact_kitty_packet() {
    let mut upload =
        KittyUpload::new(image(1, 1, vec![1, 2, 3, 4]), 7).expect("the image is valid");
    let mut output = Vec::new();

    upload.advance(&mut output).expect("the upload writes");

    assert_eq!(
        output,
        b"\x1b_Ga=t,f=32,s=1,v=1,I=7,q=2,o=z,m=0;eAFjZGJmAQAAGAAL\x1b\\"
    );
    assert!(upload.started());
    assert!(upload.complete());
}

#[test]
fn upload_retains_the_caller_arc_and_exposes_its_identity() {
    let pixels = image(1, 1, vec![1, 2, 3, 4]);
    let upload = KittyUpload::new(Arc::clone(&pixels), 11).expect("the image is valid");

    assert!(Arc::ptr_eq(upload.image(), &pixels));
    assert_eq!(upload.image_number(), 11);
    assert!(!upload.started());
    assert!(!upload.complete());
}

#[test]
fn upload_rejects_zero_image_number() {
    assert!(matches!(
        KittyUpload::new(image(1, 1, vec![1, 2, 3, 4]), 0),
        Err(KittyOutputError::InvalidImageNumber)
    ));
}

#[test]
fn upload_rejects_invalid_dimensions_and_rgba_length() {
    assert!(matches!(
        KittyUpload::new(image(0, 1, Vec::new()), 1),
        Err(KittyOutputError::InvalidImageDimensions {
            width: 0,
            height: 1,
        })
    ));
    assert!(matches!(
        KittyUpload::new(image(16_385, 1, Vec::new()), 1),
        Err(KittyOutputError::InvalidImageDimensions {
            width: 16_385,
            height: 1,
        })
    ));
    assert!(matches!(
        KittyUpload::new(image(1, 1, vec![1]), 1),
        Err(KittyOutputError::InvalidImageData {
            width: 1,
            height: 1,
            expected: 4,
            actual: 1,
        })
    ));
}

#[test]
fn upload_reports_a_writer_failure_at_a_packet_boundary_without_progress() {
    let pixels = random_rgba(16_384);
    let mut expected_upload =
        KittyUpload::new(image(4_096, 1, pixels.clone()), 3).expect("the image is valid");
    let mut expected = Vec::new();
    expected_upload
        .advance(&mut expected)
        .expect("the complete output step writes");
    let first_packet_end = expected
        .windows(2)
        .position(|bytes| bytes == b"\x1b\\")
        .expect("the output has a packet terminator")
        + 2;

    let mut upload = KittyUpload::new(image(4_096, 1, pixels), 3).expect("the image is valid");
    let mut writer = FailAfter {
        output: Vec::new(),
        limit: first_packet_end,
    };
    let error = upload
        .advance(&mut writer)
        .expect_err("the second packet write fails");

    assert!(matches!(
        error,
        KittyOutputError::Io(ref error) if error.kind() == io::ErrorKind::BrokenPipe
    ));
    assert_eq!(writer.output, expected[..first_packet_end]);
    assert!(!upload.started());
    assert!(!upload.complete());
}

#[test]
fn one_advance_compresses_at_most_256_kibibytes_of_input() {
    let mut upload =
        KittyUpload::new(image(16_384, 32, vec![0; 2_097_152]), 1).expect("the image is valid");
    let mut output = Vec::new();

    upload
        .advance(&mut output)
        .expect("the bounded step writes");

    assert_eq!(upload.input_offset, KITTY_COMPRESSION_INPUT_BYTES_PER_STEP);
    assert!(!upload.compression_complete);
}

#[test]
fn one_advance_writes_at_most_sixteen_compressed_chunks() {
    let record = image(1, 1, vec![1, 2, 3, 4]);
    let mut upload = KittyUpload {
        image: record,
        image_number: 7,
        compressor: Compress::new(Compression::fast(), true),
        input_offset: 4,
        compressed: vec![0x5a; KITTY_IMAGE_CHUNK_BYTES * (KITTY_IMAGE_CHUNKS_PER_STEP + 1)],
        compressed_offset: 0,
        compression_complete: true,
        transmission_started: false,
    };
    let mut output = Vec::new();

    upload
        .advance(&mut output)
        .expect("the bounded step writes");

    assert_eq!(
        upload.compressed_offset,
        KITTY_IMAGE_CHUNK_BYTES * KITTY_IMAGE_CHUNKS_PER_STEP
    );
    assert!(upload.started());
    assert!(!upload.complete());
    assert_eq!(
        output
            .windows(b"\x1b_G".len())
            .filter(|bytes| *bytes == b"\x1b_G")
            .count(),
        KITTY_IMAGE_CHUNKS_PER_STEP
    );
    assert!(output.windows(4).all(|bytes| bytes != b"m=0;"));
}

#[test]
fn placement_writes_source_offsets_and_z_index() {
    let image = DecodedImage {
        width: 4,
        height: 3,
        rgba: vec![0; 4 * 3 * 4],
    };
    let placement = KittyPlacement {
        image_number: 7,
        placement_id: 9,
        source_x: 1,
        source_y: 2,
        source_width: 2,
        source_height: 1,
        columns: 3,
        rows: 4,
        cell_offset_x: Some(5),
        cell_offset_y: Some(6),
        z_index: -2,
    };
    let mut output = Vec::new();

    write_kitty_placement(&mut output, &image, &placement).expect("the placement writes");

    assert_eq!(
        output,
        b"\x1b_Ga=p,I=7,p=9,x=1,y=2,w=2,h=1,X=5,Y=6,c=3,r=4,C=1,z=-2,q=2;\x1b\\"
    );
}

#[test]
fn placement_rejects_zero_ids_dimensions_and_out_of_bounds_source() {
    let image = DecodedImage {
        width: 2,
        height: 2,
        rgba: vec![0; 16],
    };
    let valid = KittyPlacement {
        image_number: 1,
        placement_id: 1,
        source_x: 0,
        source_y: 0,
        source_width: 1,
        source_height: 1,
        columns: 1,
        rows: 1,
        cell_offset_x: None,
        cell_offset_y: None,
        z_index: 0,
    };
    let mut output = Vec::new();

    let mut placement = valid;
    placement.image_number = 0;
    assert!(matches!(
        write_kitty_placement(&mut output, &image, &placement),
        Err(KittyOutputError::InvalidImageNumber)
    ));
    placement = valid;
    placement.placement_id = 0;
    assert!(matches!(
        write_kitty_placement(&mut output, &image, &placement),
        Err(KittyOutputError::InvalidPlacementId)
    ));
    placement = valid;
    placement.columns = 0;
    assert!(matches!(
        write_kitty_placement(&mut output, &image, &placement),
        Err(KittyOutputError::InvalidPlacementDimensions {
            columns: 0,
            rows: 1,
        })
    ));
    placement = valid;
    placement.source_x = 2;
    assert!(matches!(
        write_kitty_placement(&mut output, &image, &placement),
        Err(KittyOutputError::InvalidSourceRect {
            x: 2,
            y: 0,
            width: 1,
            height: 1,
            image_width: 2,
            image_height: 2,
        })
    ));
}

#[test]
fn delete_commands_write_exact_kitty_bytes() {
    let mut output = Vec::new();

    write_kitty_image_delete(&mut output, 7).expect("the image delete writes");
    write_kitty_placement_delete(&mut output, 7, 9).expect("the placement delete writes");
    write_kitty_delete_all(&mut output).expect("the all delete writes");
    write_kitty_visible_placement_delete(&mut output).expect("the placement-only delete writes");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=N,I=7,q=2;\x1b\\\x1b_Ga=d,d=n,I=7,p=9,q=2;\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\"
    );
}

#[test]
fn abort_writes_the_exact_open_transfer_cancellation_bytes() {
    let mut output = Vec::new();

    write_kitty_abort(&mut output).expect("the abort writes");

    assert_eq!(output, b"\x18\x1b\\");
}

#[test]
fn delete_commands_reject_zero_ids() {
    let mut output = Vec::new();

    assert!(matches!(
        write_kitty_image_delete(&mut output, 0),
        Err(KittyOutputError::InvalidImageNumber)
    ));
    assert!(matches!(
        write_kitty_placement_delete(&mut output, 0, 1),
        Err(KittyOutputError::InvalidImageNumber)
    ));
    assert!(matches!(
        write_kitty_placement_delete(&mut output, 1, 0),
        Err(KittyOutputError::InvalidPlacementId)
    ));
    assert!(output.is_empty());
}
