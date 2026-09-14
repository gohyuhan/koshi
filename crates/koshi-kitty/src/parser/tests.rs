//! Tests for Kitty payload parsing and image decoding.

use super::*;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::ImageEncoder;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;

fn parse_kitty_chunk(body_bytes: &[u8]) -> Result<KittyChunk, GraphicsError> {
    let mut parser = KittyParser::new();
    for &input_byte in body_bytes {
        parser
            .feed_input_byte(input_byte)
            .expect("the Kitty body is valid");
    }
    parser.finish_kitty_chunk()
}

fn build_red_png_bytes() -> Vec<u8> {
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the PNG encodes");
    png_bytes
}

fn build_png_chunk_bytes(chunk_type: &[u8; 4], chunk_payload_bytes: &[u8]) -> Vec<u8> {
    let mut png_chunk_bytes = Vec::with_capacity(chunk_payload_bytes.len() + 12);
    png_chunk_bytes.extend_from_slice(&(chunk_payload_bytes.len() as u32).to_be_bytes());
    png_chunk_bytes.extend_from_slice(chunk_type);
    png_chunk_bytes.extend_from_slice(chunk_payload_bytes);
    let mut crc = 0xffff_ffffu32;
    for &png_byte in chunk_type.iter().chain(chunk_payload_bytes) {
        crc ^= u32::from(png_byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    png_chunk_bytes.extend_from_slice(&(!crc).to_be_bytes());
    png_chunk_bytes
}

fn extract_png_image_data_bytes(png_bytes: &[u8]) -> Vec<u8> {
    let mut image_data_bytes = Vec::new();
    let mut png_byte_offset = 8;
    while png_byte_offset < png_bytes.len() {
        let png_chunk_byte_count = usize::try_from(u32::from_be_bytes(
            png_bytes[png_byte_offset..png_byte_offset + 4]
                .try_into()
                .expect("PNG chunk length"),
        ))
        .expect("PNG chunk length fits");
        let png_chunk_end_byte_offset = png_byte_offset + 12 + png_chunk_byte_count;
        if &png_bytes[png_byte_offset + 4..png_byte_offset + 8] == b"IDAT" {
            image_data_bytes
                .extend_from_slice(&png_bytes[png_byte_offset + 8..png_chunk_end_byte_offset - 4]);
        }
        png_byte_offset = png_chunk_end_byte_offset;
    }
    image_data_bytes
}

fn build_animated_png_bytes(default_pixel_rgba: [u8; 4], frame_pixel_rgba: [u8; 4]) -> Vec<u8> {
    let build_png_bytes = |pixel_rgba: [u8; 4]| {
        let mut png_bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png_bytes)
            .write_image(&pixel_rgba, 1, 1, image::ColorType::Rgba8.into())
            .expect("the PNG encodes");
        png_bytes
    };
    let default_png_bytes = build_png_bytes(default_pixel_rgba);
    let frame_png_bytes = build_png_bytes(frame_pixel_rgba);
    let mut frame_control_bytes = [0; 26];
    frame_control_bytes[4..8].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[8..12].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame_control_bytes[22..24].copy_from_slice(&10u16.to_be_bytes());
    let mut animation_frame_data_bytes = 1u32.to_be_bytes().to_vec();
    animation_frame_data_bytes.extend_from_slice(&extract_png_image_data_bytes(&frame_png_bytes));

    let mut animated_png_bytes = default_png_bytes[..33].to_vec();
    animated_png_bytes
        .extend_from_slice(&build_png_chunk_bytes(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    animated_png_bytes.extend_from_slice(&build_png_chunk_bytes(
        b"IDAT",
        &extract_png_image_data_bytes(&default_png_bytes),
    ));
    animated_png_bytes.extend_from_slice(&build_png_chunk_bytes(b"fcTL", &frame_control_bytes));
    animated_png_bytes
        .extend_from_slice(&build_png_chunk_bytes(b"fdAT", &animation_frame_data_bytes));
    animated_png_bytes.extend_from_slice(&build_png_chunk_bytes(b"IEND", &[]));
    animated_png_bytes
}

fn build_red_jpeg_bytes() -> Vec<u8> {
    let mut jpeg_bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg_bytes)
        .write_image(&[255, 0, 0], 1, 1, image::ColorType::Rgb8.into())
        .expect("the JPEG encodes");
    jpeg_bytes
}

fn decode_complete_transfer(body_bytes: &[u8]) -> Result<DecodedGraphics, GraphicsError> {
    let kitty_chunk = parse_kitty_chunk(body_bytes)?;
    match start_kitty_transfer(kitty_chunk)? {
        KittyTransferOutcome::Complete(decoded_graphics) => Ok(decoded_graphics),
        KittyTransferOutcome::Pending(_) => {
            panic!("the body unexpectedly starts a multipart transfer")
        }
    }
}

fn build_red_rgba_pixel() -> [u8; 4] {
    [255, 0, 0, 255]
}

fn build_unique_path(path_prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{path_prefix}-{}-{nonce}", std::process::id()))
}

fn encode_source_path(source_path: &Path) -> String {
    STANDARD.encode(source_path.to_string_lossy().as_bytes())
}

fn parse_kitty_animation_command(command_body: &str) -> Result<KittyCommand, GraphicsError> {
    let (control_header_bytes, encoded_payload_text) = command_body
        .split_once(';')
        .expect("the Kitty body has a payload");
    parse_kitty_command(
        control_header_bytes.as_bytes(),
        encoded_payload_text.as_bytes(),
    )
    .expect("the body is an animation command")
}

fn write_source_file(source_path: &Path, source_bytes: &[u8]) {
    fs::write(source_path, source_bytes).expect("the test source writes");
}

fn compress_payload_bytes(payload_bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(payload_bytes)
        .expect("the test payload compresses");
    encoder.finish().expect("the compressed payload finishes")
}

fn parse_animation_chunk(body_bytes: &[u8]) -> KittyAnimationChunk {
    let mut parser = KittyParser::new();
    for &input_byte in body_bytes {
        parser
            .feed_input_byte(input_byte)
            .expect("the animation body is bounded");
    }
    parser
        .finish_kitty_animation_chunk()
        .expect("the animation body parses")
        .expect("the body is an animation transfer chunk")
}

#[test]
fn parser_exposes_bounded_header_and_payload_accessors() {
    let encoded_payload_text = STANDARD.encode([255, 0, 0, 255]);
    let command_body = format!("Gf=32,s=1,v=1;{encoded_payload_text}");
    let mut parser = KittyParser::new();
    for &input_byte in command_body.as_bytes() {
        parser
            .feed_input_byte(input_byte)
            .expect("the Kitty body is valid");
    }

    assert!(!parser.is_ignored());
    assert!(!parser.is_escaped());
    assert!(parser.has_received_control_header());
    assert_eq!(parser.get_control_header_bytes(), b"Gf=32,s=1,v=1");
    assert_eq!(parser.get_payload_bytes(), encoded_payload_text.as_bytes());
    assert_eq!(parser.get_payload_byte_count(), encoded_payload_text.len());
}

#[test]
fn parser_appends_payload_runs_with_the_same_limit() {
    let mut parser = KittyParser::new();
    parser
        .feed_input_byte(b'G')
        .expect("the Kitty introducer is valid");
    parser
        .feed_input_byte(b';')
        .expect("the empty control bytes are valid");
    parser
        .append_payload_bytes(&vec![b'A'; MAX_KITTY_CHUNK_BYTE_COUNT])
        .expect("one chunk stays within the bound");
    assert_eq!(parser.get_payload_byte_count(), MAX_KITTY_CHUNK_BYTE_COUNT);
    assert_eq!(
        parser.append_payload_bytes(b"A"),
        Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn raw_rgba_transfer_decodes_exact_pixels() {
    let encoded_payload_text = STANDARD.encode([255, 0, 0, 255]);
    let command_body = format!("Gf=32,s=1,v=1;{encoded_payload_text}");

    let decoded_graphics =
        decode_complete_transfer(command_body.as_bytes()).expect("the RGBA transfer decodes");

    assert_eq!(decoded_graphics.protocol, KITTY_PROTOCOL);
    assert_eq!(decoded_graphics.action, ImageAction::Transmit);
    assert_eq!(decoded_graphics.image.pixel_width, 1);
    assert_eq!(decoded_graphics.image.pixel_height, 1);
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn png_transfer_decodes_png_data() {
    let encoded_payload_text = STANDARD.encode(build_red_png_bytes());
    let command_body = format!("Gf=100;{encoded_payload_text}");

    let decoded_graphics =
        decode_complete_transfer(command_body.as_bytes()).expect("the PNG transfer decodes");

    assert_eq!(decoded_graphics.image.pixel_width, 1);
    assert_eq!(decoded_graphics.image.pixel_height, 1);
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn root_apng_transfer_decodes_only_its_default_image() {
    let encoded_payload_text =
        STANDARD.encode(build_animated_png_bytes([255, 0, 0, 255], [0, 255, 0, 255]));
    let command_body = format!("Gf=100;{encoded_payload_text}");

    let decoded_graphics =
        decode_complete_transfer(command_body.as_bytes()).expect("the APNG transfer decodes");

    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
    assert_eq!(decoded_graphics.animation, None);
}

#[test]
fn direct_transfer_size_is_not_a_raw_or_png_payload_length() {
    let raw_decoded_graphics = decode_complete_transfer(
        format!(
            "Gf=32,s=1,v=1,S=99;{}",
            STANDARD.encode(build_red_rgba_pixel())
        )
        .as_bytes(),
    )
    .expect("the raw transfer ignores the size hint");
    assert_eq!(
        raw_decoded_graphics.image.rgba_bytes,
        build_red_rgba_pixel()
    );

    let png_bytes = build_red_png_bytes();
    let png_decoded_graphics =
        decode_complete_transfer(format!("Gf=100,S=99;{}", STANDARD.encode(&png_bytes)).as_bytes())
            .expect("the PNG transfer uses the size as an allocation hint");
    assert_eq!(
        png_decoded_graphics.image.rgba_bytes,
        build_red_rgba_pixel()
    );

    let compressed_rgba_bytes = compress_payload_bytes(&build_red_rgba_pixel());
    let compressed_decoded_graphics = decode_complete_transfer(
        format!(
            "Gf=32,s=1,v=1,o=z,S=99;{}",
            STANDARD.encode(compressed_rgba_bytes)
        )
        .as_bytes(),
    )
    .expect("raw decompression uses the dimensions");
    assert_eq!(
        compressed_decoded_graphics.image.rgba_bytes,
        build_red_rgba_pixel()
    );
}

#[test]
fn png_transfer_rejects_mislabeled_jpeg_bytes() {
    let encoded_payload_text = STANDARD.encode(build_red_jpeg_bytes());
    let command_body = format!("Gf=100;{encoded_payload_text}");

    assert_eq!(
        decode_complete_transfer(command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn png_compression_requires_declared_uncompressed_size() {
    assert_eq!(
        decode_complete_transfer(b"Gf=100,o=z;AAAA"),
        Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn oversized_raw_dimensions_are_rejected_before_payload_decode() {
    assert_eq!(
        decode_complete_transfer(b"Gf=32,s=4097,v=4097;AAAA"),
        Err(GraphicsError::ImageTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn multipart_transfer_decodes_only_after_the_final_chunk() {
    let encoded_payload_text = STANDARD.encode([255, 0, 0, 255]);
    let first_chunk =
        parse_kitty_chunk(format!("Gf=32,s=1,v=1,m=1;{}", &encoded_payload_text[..4]).as_bytes())
            .expect("the first chunk is valid");
    let final_chunk = parse_kitty_chunk(format!("Gm=0;{}", &encoded_payload_text[4..]).as_bytes())
        .expect("the second chunk is valid");
    let KittyTransferOutcome::Pending(pending_transfer) =
        start_kitty_transfer(first_chunk).expect("first chunk")
    else {
        panic!("the first chunk completed unexpectedly")
    };

    let KittyTransferOutcome::Complete(decoded_graphics) = pending_transfer
        .accept_continuation_chunk(final_chunk)
        .expect("the final chunk completes")
    else {
        panic!("the final chunk remained pending")
    };
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn continuation_rejects_non_chunk_control_fields() {
    let encoded_payload_text = STANDARD.encode([255, 0, 0, 255]);
    let first_chunk =
        parse_kitty_chunk(format!("Gf=32,s=1,v=1,m=1;{}", &encoded_payload_text[..4]).as_bytes())
            .expect("the first chunk is valid");
    let final_chunk =
        parse_kitty_chunk(format!("Gf=32,m=0;{}", &encoded_payload_text[4..]).as_bytes())
            .expect("the second chunk is valid");
    let KittyTransferOutcome::Pending(pending_transfer) =
        start_kitty_transfer(first_chunk).expect("first chunk")
    else {
        panic!("the first chunk completed unexpectedly")
    };

    let transfer_outcome = pending_transfer.accept_continuation_chunk(final_chunk);
    match transfer_outcome {
        Err(graphics_error) => assert_eq!(
            graphics_error,
            GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            }
        ),
        Ok(_) => panic!("a non-chunk control field was accepted"),
    }
}

#[test]
fn regular_file_transfer_reads_only_the_requested_range() {
    let source_path = build_unique_path("kitty-image-source");
    let mut source_bytes = b"prefix".to_vec();
    source_bytes.extend_from_slice(&build_red_rgba_pixel());
    source_bytes.extend_from_slice(b"suffix");
    write_source_file(&source_path, &source_bytes);
    let command_body = format!(
        "Gf=32,s=1,v=1,t=f,S=4,O=6;{}",
        encode_source_path(&source_path)
    );

    let decoded_graphics = decode_complete_transfer(command_body.as_bytes())
        .expect("the regular file transfer decodes");

    assert_eq!(decoded_graphics.image.rgba_bytes, build_red_rgba_pixel());
    assert!(source_path.exists(), "t=f must keep the source file");
    fs::remove_file(source_path).expect("the test source removes");
}

#[test]
fn disposable_file_transfer_deletes_an_approved_source() {
    let source_path = build_unique_path("tty-graphics-protocol-image");
    write_source_file(&source_path, &build_red_rgba_pixel());
    let command_body = format!("Gf=32,s=1,v=1,t=t;{}", encode_source_path(&source_path));

    let decoded_graphics = decode_complete_transfer(command_body.as_bytes())
        .expect("the disposable file transfer decodes");

    assert_eq!(decoded_graphics.image.rgba_bytes, build_red_rgba_pixel());
    assert!(
        !source_path.exists(),
        "t=t removes the approved disposable file"
    );
}

#[test]
fn disposable_file_transfer_keeps_an_unapproved_source() {
    let source_path = build_unique_path("kitty-image-source");
    write_source_file(&source_path, &build_red_rgba_pixel());
    let command_body = format!("Gf=32,s=1,v=1,t=t;{}", encode_source_path(&source_path));

    assert_eq!(
        decode_complete_transfer(command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(
        source_path.exists(),
        "an unapproved t=t file is not removed"
    );
    fs::remove_file(source_path).expect("the test source removes");
}

#[test]
fn disposable_file_transfer_removes_the_source_when_reading_fails() {
    let source_path = build_unique_path("tty-graphics-protocol-image");
    write_source_file(&source_path, &build_red_rgba_pixel());
    let command_body = format!("Gf=32,s=1,v=1,t=t,S=5;{}", encode_source_path(&source_path));

    assert_eq!(
        decode_complete_transfer(command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(
        !source_path.exists(),
        "t=t removes the source after a read error"
    );
}

#[cfg(unix)]
#[test]
fn disposable_file_transfer_reads_and_deletes_one_resolved_path() {
    use std::os::unix::fs::symlink;

    let target_path = build_unique_path("tty-graphics-protocol-image");
    let alias_path = build_unique_path("kitty-image-alias");
    write_source_file(&target_path, &build_red_rgba_pixel());
    symlink(&target_path, &alias_path).expect("the test alias creates");
    let command_body = format!("Gf=32,s=1,v=1,t=t;{}", encode_source_path(&alias_path));

    let decoded_graphics =
        decode_complete_transfer(command_body.as_bytes()).expect("the disposable alias decodes");

    assert_eq!(decoded_graphics.image.rgba_bytes, build_red_rgba_pixel());
    assert!(!target_path.exists());
    assert!(fs::symlink_metadata(&alias_path).is_ok());
    fs::remove_file(alias_path).expect("the test alias removes");
}

#[test]
fn external_transfer_rejects_bad_names_non_regular_files_and_out_of_bounds_ranges() {
    assert_eq!(
        decode_complete_transfer(b"Gf=32,s=1,v=1,t=f;%%%"),
        Err(GraphicsError::InvalidBase64 {
            protocol: KITTY_PROTOCOL,
        })
    );

    let directory_path = build_unique_path("kitty-image-directory");
    fs::create_dir(&directory_path).expect("the test directory creates");
    let directory_command_body =
        format!("Gf=32,s=1,v=1,t=f;{}", encode_source_path(&directory_path));
    assert_eq!(
        decode_complete_transfer(directory_command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_dir(directory_path).expect("the test directory removes");

    let source_path = build_unique_path("kitty-image-source");
    write_source_file(&source_path, &build_red_rgba_pixel());
    let out_of_bounds_command_body =
        format!("Gf=32,s=1,v=1,t=f,O=5;{}", encode_source_path(&source_path));
    assert_eq!(
        decode_complete_transfer(out_of_bounds_command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_file(source_path).expect("the test source removes");
}

#[cfg(unix)]
#[test]
fn external_fifo_and_socket_sources_are_rejected_without_blocking() {
    use std::os::unix::net::UnixListener;

    let fifo_path = build_unique_path("tty-graphics-protocol-fifo");
    let fifo_path_cstring =
        CString::new(fifo_path.as_os_str().as_encoded_bytes()).expect("FIFO path");
    assert_eq!(
        unsafe { libc::mkfifo(fifo_path_cstring.as_ptr(), 0o600) },
        0
    );
    let fifo_command_body = format!("Gf=32,s=1,v=1,t=t;{}", encode_source_path(&fifo_path));
    let fifo_read_start_time = Instant::now();
    assert_eq!(
        decode_complete_transfer(fifo_command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(fifo_read_start_time.elapsed() < Duration::from_secs(1));
    assert!(!fifo_path.exists(), "an opened disposable FIFO is removed");

    let socket_path = build_unique_path("kitty-image-socket");
    let socket_listener = UnixListener::bind(&socket_path).expect("the Unix socket binds");
    let socket_command_body = format!("Gf=32,s=1,v=1,t=f;{}", encode_source_path(&socket_path));
    let socket_read_start_time = Instant::now();
    assert_eq!(
        decode_complete_transfer(socket_command_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(socket_read_start_time.elapsed() < Duration::from_secs(1));
    drop(socket_listener);
    fs::remove_file(socket_path).expect("the Unix socket removes");

    let decoded_graphics = decode_complete_transfer(
        format!("Gf=32,s=1,v=1;{}", STANDARD.encode(build_red_rgba_pixel())).as_bytes(),
    )
    .expect("a rejected external source does not affect the next payload");
    assert_eq!(decoded_graphics.image.rgba_bytes, build_red_rgba_pixel());
}

#[test]
fn external_transfer_rejects_a_source_larger_than_the_bound_before_allocating() {
    let source_path = build_unique_path("kitty-image-source");
    let source_file = File::create(&source_path).expect("the test source creates");
    source_file
        .set_len((MAX_GRAPHICS_TRANSFER_BYTE_COUNT as u64) + 1)
        .expect("the sparse source sizes");
    drop(source_file);
    let command_body = format!("Gf=32,s=1,v=1,t=f;{}", encode_source_path(&source_path));

    assert_eq!(
        decode_complete_transfer(command_body.as_bytes()),
        Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_file(source_path).expect("the test source removes");
}

#[cfg(unix)]
#[test]
fn shared_memory_transfer_reads_and_unlinks_the_object() {
    let shared_memory_name = format!(
        "/tty-graphics-protocol-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_nanos()
    );
    let shared_memory_name_cstring =
        CString::new(shared_memory_name.clone()).expect("the shared-memory name has no NUL");
    let shared_memory_descriptor = unsafe {
        libc::shm_open(
            shared_memory_name_cstring.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if shared_memory_descriptor < 0 {
        return;
    }
    let mut shared_memory_file = unsafe { File::from_raw_fd(shared_memory_descriptor) };
    let mut shared_memory_bytes = build_red_rgba_pixel().to_vec();
    shared_memory_bytes.extend_from_slice(&[0; 32]);
    shared_memory_file
        .write_all(&shared_memory_bytes)
        .expect("the shared-memory object writes");
    shared_memory_file
        .flush()
        .expect("the shared-memory object flushes");
    drop(shared_memory_file);
    let encoded_shared_memory_name = STANDARD.encode(shared_memory_name.as_bytes());
    let command_body = format!("Gf=32,s=1,v=1,t=s;{encoded_shared_memory_name}");

    let decoded_graphics = decode_complete_transfer(command_body.as_bytes())
        .expect("the shared-memory transfer decodes");

    assert_eq!(decoded_graphics.image.rgba_bytes, build_red_rgba_pixel());
    let reopened_shared_memory_descriptor =
        unsafe { libc::shm_open(shared_memory_name_cstring.as_ptr(), libc::O_RDONLY, 0) };
    assert_eq!(
        reopened_shared_memory_descriptor, -1,
        "the shared-memory object is unlinked"
    );
}

#[test]
fn shared_memory_payload_without_size_ignores_mapping_padding() {
    let mut raw_rgba_bytes = build_red_rgba_pixel().to_vec();
    raw_rgba_bytes.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(&raw_rgba_bytes, KittyFormat::Rgba, Some(1), Some(1), false,),
        Ok(build_red_rgba_pixel().to_vec())
    );

    let png_bytes = build_red_png_bytes();
    let mut padded_png_bytes = png_bytes.clone();
    padded_png_bytes.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(&padded_png_bytes, KittyFormat::Png, None, None, false),
        Ok(png_bytes)
    );

    let compressed_payload_bytes = compress_payload_bytes(&build_red_rgba_pixel());
    let mut padded_compressed_payload_bytes = compressed_payload_bytes.clone();
    padded_compressed_payload_bytes.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(
            &padded_compressed_payload_bytes,
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            true,
        ),
        Ok(build_red_rgba_pixel().to_vec())
    );
}

#[test]
fn shared_memory_rgb_payload_without_size_uses_three_channels() {
    assert_eq!(
        exact_shared_memory_payload(&[1, 2, 3], KittyFormat::Rgb, Some(1), Some(1), false),
        Ok(vec![1, 2, 3])
    );
}

#[test]
fn shared_memory_payload_without_size_rejects_truncated_data() {
    assert_eq!(
        exact_shared_memory_payload(
            &build_red_rgba_pixel()[..3],
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            false
        ),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    let png_bytes = build_red_png_bytes();
    assert_eq!(
        exact_shared_memory_payload(
            &png_bytes[..png_bytes.len() - 1],
            KittyFormat::Png,
            None,
            None,
            false,
        ),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    let compressed_payload_bytes = compress_payload_bytes(&build_red_rgba_pixel());
    assert_eq!(
        exact_shared_memory_payload(
            &compressed_payload_bytes[..compressed_payload_bytes.len() - 1],
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            true,
        ),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
}

#[test]
fn animation_frame_accepts_an_external_compressed_file() {
    let raw_rgba_bytes = build_red_rgba_pixel();
    let compressed_payload_bytes = compress_payload_bytes(&raw_rgba_bytes);
    let source_path = build_unique_path("kitty-animation-source");
    write_source_file(&source_path, &compressed_payload_bytes);
    let command_body = format!(
        "Ga=f,i=7,f=32,s=1,v=1,r=1,t=f,o=z,S={};{}",
        compressed_payload_bytes.len(),
        encode_source_path(&source_path)
    );

    let animation_command = parse_kitty_animation_command(&command_body)
        .expect("the external animation frame is valid");

    assert_eq!(
        animation_command
            .get_animation_command()
            .expect("animation fields")
            .encoded_payload_bytes,
        STANDARD.encode(raw_rgba_bytes).into_bytes()
    );
    assert!(source_path.exists(), "t=f keeps the animation source");
    fs::remove_file(source_path).expect("the test source removes");
}

#[test]
fn animation_frame_accepts_compressed_direct_data() {
    let raw_rgba_bytes = build_red_rgba_pixel();
    let compressed_payload_bytes = compress_payload_bytes(&raw_rgba_bytes);
    let encoded_payload_text = STANDARD.encode(&compressed_payload_bytes);
    let command_body = format!(
        "Ga=f,i=7,f=32,s=1,v=1,r=1,o=z,S={};{encoded_payload_text}",
        raw_rgba_bytes.len()
    );
    let animation_command = parse_kitty_animation_command(&command_body)
        .expect("the compressed animation frame is valid");

    assert_eq!(
        animation_command
            .get_animation_command()
            .expect("animation fields")
            .encoded_payload_bytes,
        STANDARD.encode(raw_rgba_bytes).into_bytes()
    );
}

#[test]
fn animation_frame_accepts_and_ignores_usage_hints() {
    let animation_command = parse_kitty_animation_command("Ga=f,i=7,f=32,s=1,v=1,N=99;/wAA/w==")
        .expect("the usage hint is accepted");

    assert_eq!(
        animation_command.get_command_kind(),
        KittyCommandKind::AnimationFrame
    );
    assert_eq!(
        animation_command
            .get_animation_command()
            .expect("animation fields")
            .encoded_payload_bytes,
        b"/wAA/w=="
    );
}

#[test]
fn compressed_png_animation_multipart_requires_a_source_size() {
    let encoded_payload_text = STANDARD.encode(compress_payload_bytes(&build_red_png_bytes()));
    let split_byte_offset = 4;
    let first_chunk = parse_animation_chunk(
        format!(
            "Ga=f,i=7,f=100,o=z,m=1;{}",
            &encoded_payload_text[..split_byte_offset]
        )
        .as_bytes(),
    );
    let KittyAnimationTransferOutcome::Pending(pending_transfer) =
        start_kitty_animation_transfer(first_chunk).expect("the bounded first chunk is retained")
    else {
        panic!("the first chunk unexpectedly completes")
    };
    let final_chunk = parse_animation_chunk(
        format!("Ga=f,m=0;{}", &encoded_payload_text[split_byte_offset..]).as_bytes(),
    );
    let Err(graphics_error) = pending_transfer.accept_continuation_chunk(final_chunk) else {
        panic!("the compressed multipart PNG unexpectedly completes")
    };
    assert_eq!(
        graphics_error,
        GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        }
    );
}

#[test]
fn uppercase_animation_frame_delete_preserves_its_free_data_flag() {
    let animation_command = parse_kitty_animation_command("Ga=d,d=F,i=7;")
        .expect("the uppercase animation delete is valid");

    assert_eq!(
        animation_command.get_command_kind(),
        KittyCommandKind::AnimationDelete
    );
    assert!(animation_command.should_free_image_data());
    assert_eq!(
        animation_command
            .get_animation_command()
            .expect("animation fields")
            .frame_number,
        None
    );
}

#[test]
fn animation_frame_direct_multipart_data_is_decoded_after_the_final_chunk() {
    let raw_rgba_bytes = build_red_rgba_pixel();
    let encoded_payload_text = STANDARD.encode(raw_rgba_bytes);
    let split_byte_offset = 4;
    let first_chunk = parse_animation_chunk(
        format!(
            "Ga=f,i=7,f=32,s=1,v=1,r=1,m=1;{}",
            &encoded_payload_text[..split_byte_offset]
        )
        .as_bytes(),
    );
    let transfer_outcome =
        start_kitty_animation_transfer(first_chunk).expect("the first frame chunk starts");
    let KittyAnimationTransferOutcome::Pending(pending_transfer) = transfer_outcome else {
        panic!("the first frame chunk completed")
    };
    let final_chunk = parse_animation_chunk(
        format!("Ga=f,m=0;{}", &encoded_payload_text[split_byte_offset..]).as_bytes(),
    );

    let KittyAnimationTransferOutcome::Complete(animation_command) = pending_transfer
        .accept_continuation_chunk(final_chunk)
        .expect("the final frame chunk completes")
    else {
        panic!("the final frame chunk remained pending")
    };
    assert_eq!(
        animation_command
            .get_animation_command()
            .expect("animation fields")
            .encoded_payload_bytes,
        encoded_payload_text.into_bytes()
    );
}

#[test]
fn animation_frame_rejects_multipart_external_data() {
    let source_path = build_unique_path("kitty-animation-source");
    write_source_file(&source_path, &build_red_rgba_pixel());
    let command_body = format!(
        "Ga=f,i=7,f=32,s=1,v=1,r=1,t=f,m=1;{}",
        encode_source_path(&source_path)
    );
    let mut parser = KittyParser::new();
    for &input_byte in command_body.as_bytes() {
        parser
            .feed_input_byte(input_byte)
            .expect("the animation body is bounded");
    }

    assert_eq!(
        parser.finish_kitty_animation_chunk(),
        Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(source_path.exists());
    fs::remove_file(source_path).expect("the test source removes");
}
