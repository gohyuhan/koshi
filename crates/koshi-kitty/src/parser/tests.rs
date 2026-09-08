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

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() + 12);
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for &byte in kind.iter().chain(data) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes
}

fn png_image_data(png: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut at = 8;
    while at < png.len() {
        let length = usize::try_from(u32::from_be_bytes(
            png[at..at + 4].try_into().expect("PNG chunk length"),
        ))
        .expect("PNG chunk length fits");
        let end = at + 12 + length;
        if &png[at + 4..at + 8] == b"IDAT" {
            data.extend_from_slice(&png[at + 8..end - 4]);
        }
        at = end;
    }
    data
}

fn animated_png(default: [u8; 4], frame: [u8; 4]) -> Vec<u8> {
    let encode = |rgba: [u8; 4]| {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&rgba, 1, 1, image::ColorType::Rgba8.into())
            .expect("the PNG encodes");
        bytes
    };
    let default_png = encode(default);
    let frame_png = encode(frame);
    let mut frame_control = [0; 26];
    frame_control[4..8].copy_from_slice(&1u32.to_be_bytes());
    frame_control[8..12].copy_from_slice(&1u32.to_be_bytes());
    frame_control[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame_control[22..24].copy_from_slice(&10u16.to_be_bytes());
    let mut animation_data = 1u32.to_be_bytes().to_vec();
    animation_data.extend_from_slice(&png_image_data(&frame_png));

    let mut bytes = default_png[..33].to_vec();
    bytes.extend_from_slice(&png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    bytes.extend_from_slice(&png_chunk(b"IDAT", &png_image_data(&default_png)));
    bytes.extend_from_slice(&png_chunk(b"fcTL", &frame_control));
    bytes.extend_from_slice(&png_chunk(b"fdAT", &animation_data));
    bytes.extend_from_slice(&png_chunk(b"IEND", &[]));
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

fn rgba_pixel() -> [u8; 4] {
    [255, 0, 0, 255]
}

fn unique_path(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
}

fn encoded_path(path: &Path) -> String {
    STANDARD.encode(path.to_string_lossy().as_bytes())
}

fn animation_command(body: &str) -> Result<KittyCommand, GraphicsError> {
    let (header, payload) = body.split_once(';').expect("the Kitty body has a payload");
    parse_command(header.as_bytes(), payload.as_bytes()).expect("the body is an animation command")
}

fn write_source(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("the test source writes");
}

fn compressed(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .expect("the test payload compresses");
    encoder.finish().expect("the compressed payload finishes")
}

fn animation_chunk(body: &[u8]) -> KittyAnimationChunk {
    let mut parser = KittyParser::new();
    for &byte in body {
        parser.feed(byte).expect("the animation body is bounded");
    }
    parser
        .finish_animation_chunk()
        .expect("the animation body parses")
        .expect("the body is an animation transfer chunk")
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
fn root_apng_transfer_decodes_only_its_default_image() {
    let payload = STANDARD.encode(animated_png([255, 0, 0, 255], [0, 255, 0, 255]));
    let body = format!("Gf=100;{payload}");

    let image = complete(body.as_bytes()).expect("the APNG transfer decodes");

    assert_eq!(image.image.rgba, [255, 0, 0, 255]);
    assert_eq!(image.animation, None);
}

#[test]
fn direct_transfer_size_is_not_a_raw_or_png_payload_length() {
    let raw = complete(format!("Gf=32,s=1,v=1,S=99;{}", STANDARD.encode(rgba_pixel())).as_bytes())
        .expect("the raw transfer ignores the size hint");
    assert_eq!(raw.image.rgba, rgba_pixel());

    let png = red_png();
    let decoded = complete(format!("Gf=100,S=99;{}", STANDARD.encode(&png)).as_bytes())
        .expect("the PNG transfer uses the size as an allocation hint");
    assert_eq!(decoded.image.rgba, rgba_pixel());

    let compressed_raw = compressed(&rgba_pixel());
    let decoded =
        complete(format!("Gf=32,s=1,v=1,o=z,S=99;{}", STANDARD.encode(compressed_raw)).as_bytes())
            .expect("raw decompression uses the dimensions");
    assert_eq!(decoded.image.rgba, rgba_pixel());
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

#[test]
fn regular_file_transfer_reads_only_the_requested_range() {
    let path = unique_path("kitty-image-source");
    let mut bytes = b"prefix".to_vec();
    bytes.extend_from_slice(&rgba_pixel());
    bytes.extend_from_slice(b"suffix");
    write_source(&path, &bytes);
    let body = format!("Gf=32,s=1,v=1,t=f,S=4,O=6;{}", encoded_path(&path));

    let image = complete(body.as_bytes()).expect("the regular file transfer decodes");

    assert_eq!(image.image.rgba, rgba_pixel());
    assert!(path.exists(), "t=f must keep the source file");
    fs::remove_file(path).expect("the test source removes");
}

#[test]
fn disposable_file_transfer_deletes_an_approved_source() {
    let path = unique_path("tty-graphics-protocol-image");
    write_source(&path, &rgba_pixel());
    let body = format!("Gf=32,s=1,v=1,t=t;{}", encoded_path(&path));

    let image = complete(body.as_bytes()).expect("the disposable file transfer decodes");

    assert_eq!(image.image.rgba, rgba_pixel());
    assert!(!path.exists(), "t=t removes the approved disposable file");
}

#[test]
fn disposable_file_transfer_keeps_an_unapproved_source() {
    let path = unique_path("kitty-image-source");
    write_source(&path, &rgba_pixel());
    let body = format!("Gf=32,s=1,v=1,t=t;{}", encoded_path(&path));

    assert_eq!(
        complete(body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(path.exists(), "an unapproved t=t file is not removed");
    fs::remove_file(path).expect("the test source removes");
}

#[test]
fn disposable_file_transfer_removes_the_source_when_reading_fails() {
    let path = unique_path("tty-graphics-protocol-image");
    write_source(&path, &rgba_pixel());
    let body = format!("Gf=32,s=1,v=1,t=t,S=5;{}", encoded_path(&path));

    assert_eq!(
        complete(body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(!path.exists(), "t=t removes the source after a read error");
}

#[cfg(unix)]
#[test]
fn disposable_file_transfer_reads_and_deletes_one_resolved_path() {
    use std::os::unix::fs::symlink;

    let target = unique_path("tty-graphics-protocol-image");
    let alias = unique_path("kitty-image-alias");
    write_source(&target, &rgba_pixel());
    symlink(&target, &alias).expect("the test alias creates");
    let body = format!("Gf=32,s=1,v=1,t=t;{}", encoded_path(&alias));

    let image = complete(body.as_bytes()).expect("the disposable alias decodes");

    assert_eq!(image.image.rgba, rgba_pixel());
    assert!(!target.exists());
    assert!(fs::symlink_metadata(&alias).is_ok());
    fs::remove_file(alias).expect("the test alias removes");
}

#[test]
fn external_transfer_rejects_bad_names_non_regular_files_and_out_of_bounds_ranges() {
    assert_eq!(
        complete(b"Gf=32,s=1,v=1,t=f;%%%"),
        Err(GraphicsError::InvalidBase64 {
            protocol: KITTY_PROTOCOL,
        })
    );

    let directory = unique_path("kitty-image-directory");
    fs::create_dir(&directory).expect("the test directory creates");
    let directory_body = format!("Gf=32,s=1,v=1,t=f;{}", encoded_path(&directory));
    assert_eq!(
        complete(directory_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_dir(directory).expect("the test directory removes");

    let path = unique_path("kitty-image-source");
    write_source(&path, &rgba_pixel());
    let out_of_bounds = format!("Gf=32,s=1,v=1,t=f,O=5;{}", encoded_path(&path));
    assert_eq!(
        complete(out_of_bounds.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_file(path).expect("the test source removes");
}

#[cfg(unix)]
#[test]
fn external_fifo_and_socket_sources_are_rejected_without_blocking() {
    use std::os::unix::net::UnixListener;

    let fifo = unique_path("tty-graphics-protocol-fifo");
    let fifo_name = CString::new(fifo.as_os_str().as_encoded_bytes()).expect("FIFO path");
    assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
    let fifo_body = format!("Gf=32,s=1,v=1,t=t;{}", encoded_path(&fifo));
    let started = Instant::now();
    assert_eq!(
        complete(fifo_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!fifo.exists(), "an opened disposable FIFO is removed");

    let socket = unique_path("kitty-image-socket");
    let listener = UnixListener::bind(&socket).expect("the Unix socket binds");
    let socket_body = format!("Gf=32,s=1,v=1,t=f;{}", encoded_path(&socket));
    let started = Instant::now();
    assert_eq!(
        complete(socket_body.as_bytes()),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(listener);
    fs::remove_file(socket).expect("the Unix socket removes");

    let image = complete(format!("Gf=32,s=1,v=1;{}", STANDARD.encode(rgba_pixel())).as_bytes())
        .expect("a rejected external source does not affect the next payload");
    assert_eq!(image.image.rgba, rgba_pixel());
}

#[test]
fn external_transfer_rejects_a_source_larger_than_the_bound_before_allocating() {
    let path = unique_path("kitty-image-source");
    let file = File::create(&path).expect("the test source creates");
    file.set_len((MAX_GRAPHICS_TRANSFER_BYTES as u64) + 1)
        .expect("the sparse source sizes");
    drop(file);
    let body = format!("Gf=32,s=1,v=1,t=f;{}", encoded_path(&path));

    assert_eq!(
        complete(body.as_bytes()),
        Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        })
    );
    fs::remove_file(path).expect("the test source removes");
}

#[cfg(unix)]
#[test]
fn shared_memory_transfer_reads_and_unlinks_the_object() {
    let name = format!(
        "/tty-graphics-protocol-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_nanos()
    );
    let name_c = CString::new(name.clone()).expect("the shared-memory name has no NUL");
    let descriptor = unsafe {
        libc::shm_open(
            name_c.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if descriptor < 0 {
        return;
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let mut source = rgba_pixel().to_vec();
    source.extend_from_slice(&[0; 32]);
    file.write_all(&source)
        .expect("the shared-memory object writes");
    file.flush().expect("the shared-memory object flushes");
    drop(file);
    let name_payload = STANDARD.encode(name.as_bytes());
    let body = format!("Gf=32,s=1,v=1,t=s;{name_payload}");

    let image = complete(body.as_bytes()).expect("the shared-memory transfer decodes");

    assert_eq!(image.image.rgba, rgba_pixel());
    let reopened = unsafe { libc::shm_open(name_c.as_ptr(), libc::O_RDONLY, 0) };
    assert_eq!(reopened, -1, "the shared-memory object is unlinked");
}

#[test]
fn shared_memory_payload_without_size_ignores_mapping_padding() {
    let mut raw = rgba_pixel().to_vec();
    raw.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(&raw, KittyFormat::Rgba, Some(1), Some(1), false),
        Ok(rgba_pixel().to_vec())
    );

    let png = red_png();
    let mut padded_png = png.clone();
    padded_png.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(&padded_png, KittyFormat::Png, None, None, false),
        Ok(png)
    );

    let encoded = compressed(&rgba_pixel());
    let mut padded_encoded = encoded.clone();
    padded_encoded.extend_from_slice(&[0; 32]);
    assert_eq!(
        exact_shared_memory_payload(&padded_encoded, KittyFormat::Rgba, Some(1), Some(1), true,),
        Ok(rgba_pixel().to_vec())
    );
}

#[test]
fn shared_memory_payload_without_size_rejects_truncated_data() {
    assert_eq!(
        exact_shared_memory_payload(
            &rgba_pixel()[..3],
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            false
        ),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    let png = red_png();
    assert_eq!(
        exact_shared_memory_payload(&png[..png.len() - 1], KittyFormat::Png, None, None, false),
        Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
    );
    let encoded = compressed(&rgba_pixel());
    assert_eq!(
        exact_shared_memory_payload(
            &encoded[..encoded.len() - 1],
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
    let raw = rgba_pixel();
    let compressed = compressed(&raw);
    let path = unique_path("kitty-animation-source");
    write_source(&path, &compressed);
    let body = format!(
        "Ga=f,i=7,f=32,s=1,v=1,r=1,t=f,o=z,S={};{}",
        compressed.len(),
        encoded_path(&path)
    );

    let command = animation_command(&body).expect("the external animation frame is valid");

    assert_eq!(
        command.animation().expect("animation fields").payload,
        STANDARD.encode(raw).into_bytes()
    );
    assert!(path.exists(), "t=f keeps the animation source");
    fs::remove_file(path).expect("the test source removes");
}

#[test]
fn animation_frame_accepts_compressed_direct_data() {
    let raw = rgba_pixel();
    let compressed = compressed(&raw);
    let payload = STANDARD.encode(&compressed);
    let body = format!("Ga=f,i=7,f=32,s=1,v=1,r=1,o=z,S={};{payload}", raw.len());
    let command = animation_command(&body).expect("the compressed animation frame is valid");

    assert_eq!(
        command.animation().expect("animation fields").payload,
        STANDARD.encode(raw).into_bytes()
    );
}

#[test]
fn animation_frame_accepts_and_ignores_usage_hints() {
    let command = animation_command("Ga=f,i=7,f=32,s=1,v=1,N=99;/wAA/w==")
        .expect("the usage hint is accepted");

    assert_eq!(command.kind(), KittyCommandKind::AnimationFrame);
    assert_eq!(
        command.animation().expect("animation fields").payload,
        b"/wAA/w=="
    );
}

#[test]
fn compressed_png_animation_multipart_requires_a_source_size() {
    let payload = STANDARD.encode(compressed(&red_png()));
    let split = 4;
    let first = animation_chunk(format!("Ga=f,i=7,f=100,o=z,m=1;{}", &payload[..split]).as_bytes());
    let KittyAnimationTransferOutcome::Pending(transfer) =
        start_animation_transfer(first).expect("the bounded first chunk is retained")
    else {
        panic!("the first chunk unexpectedly completes")
    };
    let second = animation_chunk(format!("Ga=f,m=0;{}", &payload[split..]).as_bytes());
    let Err(error) = transfer.accept_chunk(second) else {
        panic!("the compressed multipart PNG unexpectedly completes")
    };
    assert_eq!(
        error,
        GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        }
    );
}

#[test]
fn uppercase_animation_frame_delete_preserves_its_free_data_flag() {
    let command =
        animation_command("Ga=d,d=F,i=7;").expect("the uppercase animation delete is valid");

    assert_eq!(command.kind(), KittyCommandKind::AnimationDelete);
    assert!(command.free_data());
    assert_eq!(command.animation().expect("animation fields").frame, None);
}

#[test]
fn animation_frame_direct_multipart_data_is_decoded_after_the_final_chunk() {
    let raw = rgba_pixel();
    let encoded = STANDARD.encode(raw);
    let split = 4;
    let first =
        animation_chunk(format!("Ga=f,i=7,f=32,s=1,v=1,r=1,m=1;{}", &encoded[..split]).as_bytes());
    let transfer = start_animation_transfer(first).expect("the first frame chunk starts");
    let KittyAnimationTransferOutcome::Pending(transfer) = transfer else {
        panic!("the first frame chunk completed")
    };
    let second = animation_chunk(format!("Ga=f,m=0;{}", &encoded[split..]).as_bytes());

    let KittyAnimationTransferOutcome::Complete(command) = transfer
        .accept_chunk(second)
        .expect("the final frame chunk completes")
    else {
        panic!("the final frame chunk remained pending")
    };
    assert_eq!(
        command.animation().expect("animation fields").payload,
        encoded.into_bytes()
    );
}

#[test]
fn animation_frame_rejects_multipart_external_data() {
    let path = unique_path("kitty-animation-source");
    write_source(&path, &rgba_pixel());
    let body = format!("Ga=f,i=7,f=32,s=1,v=1,r=1,t=f,m=1;{}", encoded_path(&path));
    let mut parser = KittyParser::new();
    for &byte in body.as_bytes() {
        parser.feed(byte).expect("the animation body is bounded");
    }

    assert_eq!(
        parser.finish_animation_chunk(),
        Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        })
    );
    assert!(path.exists());
    fs::remove_file(path).expect("the test source removes");
}
