//! Rate-bounded byte pump for tests that need a slow link.
//!
//! [`pump_throttled`](throttle::pump_throttled) copies bytes from one stream to
//! another on its own thread and moves at most a fixed number of bytes per time
//! slice. A test can run one pump in each direction between a client and a
//! server.
//!
//! Example — `pump_throttled(source_reader, destination_writer, 4096, Duration::from_millis(10),
//! Instant::now() + Duration::from_secs(20))` moves at most 4096 bytes every
//! 10 milliseconds, about 400 kilobytes per second, and checks the deadline
//! 20 seconds from the current time.

use koshi_ipc::transport::is_io_timeout;
use std::io::{Read, Write};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Copy bytes from `source_reader` to `destination_writer` on a new thread, at
/// most `bytes_per_slice` bytes per `time_slice`, and return that thread's handle.
///
/// Each time slice reads once into a `bytes_per_slice`-byte buffer, writes all bytes
/// read with [`Write::write_all`], flushes `destination_writer`, and sleeps for the rest of the
/// time slice. A time slice whose read, write, and flush take longer than `time_slice` does
/// not sleep. A `bytes_per_slice` of `0` gives an empty buffer; the thread ends
/// with `0` copied after the read returns `Ok(0)`.
///
/// The thread checks `deadline` at the start of each time slice. Its handle returns
/// the total byte count when the thread ends for the first of these reasons:
///
/// - `source_reader` reports end of stream with a read of `Ok(0)`;
/// - a write or flush on `destination_writer` returns an error. The current time slice is not
///   counted, even when the write succeeds and only the flush fails;
/// - a read on `source_reader` returns an error other than
///   [`std::io::ErrorKind::WouldBlock`] or [`std::io::ErrorKind::TimedOut`];
/// - the deadline check fails. A deadline already in the past ends the thread
///   before the first read.
///
/// A read that returns `WouldBlock` or `TimedOut` pauses the pump. Unix reports
/// a read timeout as `WouldBlock`, and Windows reports it as `TimedOut`. The
/// slice copies nothing and the next slice checks `deadline` again. A source
/// without a read timeout can block in `read` past `deadline`; a blocking write
/// or flush can also delay the next deadline check.
pub fn pump_throttled(
    mut source_reader: impl Read + Send + 'static,
    mut destination_writer: impl Write + Send + 'static,
    bytes_per_slice: usize,
    time_slice: Duration,
    deadline: Instant,
) -> JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut read_buffer = vec![0_u8; bytes_per_slice];
        let mut copied_byte_count = 0_u64;
        while Instant::now() < deadline {
            let slice_start_time = Instant::now();
            match source_reader.read(&mut read_buffer) {
                Ok(0) => return copied_byte_count,
                Ok(read_byte_count) => {
                    if destination_writer
                        .write_all(&read_buffer[..read_byte_count])
                        .is_err()
                    {
                        return copied_byte_count;
                    }
                    if destination_writer.flush().is_err() {
                        return copied_byte_count;
                    }
                    copied_byte_count += read_byte_count as u64;
                }
                // A read timeout; the next loop iteration checks `deadline`.
                Err(read_error) if is_io_timeout(&read_error) => {}
                Err(_) => return copied_byte_count,
            }
            if let Some(remaining_time_slice) = time_slice.checked_sub(slice_start_time.elapsed()) {
                std::thread::sleep(remaining_time_slice);
            }
        }
        copied_byte_count
    })
}

#[cfg(test)]
mod tests;
