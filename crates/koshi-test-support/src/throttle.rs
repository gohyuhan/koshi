//! Rate-bounded byte pump for tests that need a slow link.
//!
//! [`pump_throttled`](throttle::pump_throttled) copies bytes from one stream to
//! another on its own thread, moving at most a fixed number of bytes per time
//! slice. A test puts one pump in each direction between a client and a server
//! to make the link between them slow.
//!
//! Example — `pump_throttled(reader, writer, 4096, Duration::from_millis(10),
//! Instant::now() + Duration::from_secs(20))` moves at most 4096 bytes every
//! 10 milliseconds, about 400 kilobytes per second, and stops 20 seconds
//! from now whatever the streams are doing.

use koshi_ipc::transport::waited_out;
use std::io::{Read, Write};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Copy bytes from `from` to `to` on a new thread, at most `bytes_per_slice`
/// bytes per `slice`, and hand back that thread's handle.
///
/// Each slice reads once into a `bytes_per_slice`-byte buffer, writes every
/// byte it read with [`Write::write_all`], flushes `to`, then sleeps until the
/// slice's `slice`-long span is over. A slice whose read, write and flush
/// already took longer than `slice` does not sleep. A `bytes_per_slice` of
/// `0` gives a zero-length buffer, which a stream answers with `Ok(0)`, and
/// the thread ends with `0` copied.
///
/// The thread ends, and the handle's value is the total number of bytes copied,
/// on the first of:
///
/// - `from` reporting end of stream (a read of `Ok(0)`),
/// - any write or flush error on `to`; the bytes of that slice are not
///   counted, even when the write itself succeeded and only the flush failed,
/// - a read error on `from` other than [`std::io::ErrorKind::WouldBlock`] or
///   [`std::io::ErrorKind::TimedOut`],
/// - the clock reaching `deadline`, checked at the top of every slice. A
///   `deadline` already in the past ends the thread before the first read.
///
/// A read that reports `WouldBlock` or `TimedOut` is a pause, not a failure:
/// Unix reports a read timeout as `WouldBlock` and Windows as `TimedOut`. The
/// slice ends with nothing copied and the next slice checks `deadline` again.
/// A `from` with no read timeout blocks inside `read` until bytes arrive or
/// the stream ends, past `deadline` if that takes longer.
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
                // A read timeout. The next turn of the loop checks `deadline`
                // again.
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
