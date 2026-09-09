//! Rate-bounded byte pump for tests that need a slow link.
//!
//! [`pump_throttled`](throttle::pump_throttled) copies bytes from one stream to
//! another on its own thread and moves at most a fixed number of bytes per time
//! slice. A test can run one pump in each direction between a client and a
//! server.
//!
//! Example — `pump_throttled(reader, writer, 4096, Duration::from_millis(10),
//! Instant::now() + Duration::from_secs(20))` moves at most 4096 bytes every
//! 10 milliseconds, about 400 kilobytes per second, and checks the deadline
//! 20 seconds from the current time.

use koshi_ipc::transport::waited_out;
use std::io::{Read, Write};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Copy bytes from `from` to `to` on a new thread, at most `bytes_per_slice`
/// bytes per `slice`, and return that thread's handle.
///
/// Each slice reads once into a `bytes_per_slice`-byte buffer, writes all bytes
/// read with [`Write::write_all`], flushes `to`, and sleeps for the rest of the
/// slice. A slice whose read, write, and flush take longer than `slice` does
/// not sleep. A `bytes_per_slice` of `0` gives an empty buffer; the thread ends
/// with `0` copied after the read returns `Ok(0)`.
///
/// The thread checks `deadline` at the start of each slice. Its handle returns
/// the total bytes copied when the thread ends for the first of these reasons:
///
/// - `from` reports end of stream with a read of `Ok(0)`;
/// - a write or flush on `to` returns an error. The current slice is not
///   counted, even when the write succeeds and only the flush fails;
/// - a read on `from` returns an error other than
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
    mut from: impl Read + Send + 'static,
    mut to: impl Write + Send + 'static,
    bytes_per_slice: usize,
    slice: Duration,
    deadline: Instant,
) -> JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; bytes_per_slice];
        let mut copied = 0_u64;
        while Instant::now() < deadline {
            let started = Instant::now();
            match from.read(&mut buffer) {
                Ok(0) => return copied,
                Ok(read) => {
                    if to.write_all(&buffer[..read]).is_err() {
                        return copied;
                    }
                    if to.flush().is_err() {
                        return copied;
                    }
                    copied += read as u64;
                }
                // A read timeout; the next loop iteration checks `deadline`.
                Err(error) if waited_out(&error) => {}
                Err(_) => return copied,
            }
            if let Some(left) = slice.checked_sub(started.elapsed()) {
                std::thread::sleep(left);
            }
        }
        copied
    })
}

#[cfg(test)]
mod tests;
