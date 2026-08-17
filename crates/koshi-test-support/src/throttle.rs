//! Rate-bounded byte pump for tests that need a slow link.
//!
//! [`pump_throttled`](throttle::pump_throttled) copies bytes from one stream to
//! another on its own thread, moving at most a fixed number of bytes per time
//! slice. A test puts one pump in each direction between a client and a server
//! to hold the client behind the server's output, which is what makes a
//! bounded queue on the server overflow.
//!
//! Example — `pump_throttled(reader, writer, 4096, Duration::from_millis(10),
//! Instant::now() + Duration::from_secs(20))` moves at most 4096 bytes every
//! 10 milliseconds, so about 400 kilobytes per second, and stops 20 seconds
//! from now whatever the streams are doing.

use std::io::{self, Read, Write};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Copy bytes from `from` to `to` on a new thread, at most `bytes_per_slice`
/// bytes per `slice`, and hand back that thread's handle.
///
/// Each slice reads once into a `bytes_per_slice`-byte buffer, writes every
/// byte it read with [`Write::write_all`], then sleeps until the slice's
/// `slice`-long span is over. A slice whose read and write already took longer
/// than `slice` does not sleep.
///
/// The thread ends, and the handle's value is the total number of bytes copied,
/// on the first of:
///
/// - `from` reporting end of stream (a read of `Ok(0)`),
/// - any write error on `to`,
/// - a read error on `from` other than [`io::ErrorKind::WouldBlock`] or
///   [`io::ErrorKind::TimedOut`],
/// - the clock reaching `deadline`, checked at the top of every slice.
///
/// A read that reports `WouldBlock` or `TimedOut` is a pause, not a failure:
/// Unix reports a read timeout as `WouldBlock` and Windows as `TimedOut`. The
/// slice ends with nothing copied and the next slice checks `deadline` again.
/// Give `from` a read timeout shorter than the span to `deadline`, so a peer
/// that sends nothing cannot hold the thread past `deadline`.
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
                // A read timeout: the next turn of the loop reads the clock
                // again, and the `deadline` check ends the thread.
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

/// Whether `error` is a socket read or write timeout. Unix reports one as
/// [`WouldBlock`](io::ErrorKind::WouldBlock), Windows as
/// [`TimedOut`](io::ErrorKind::TimedOut).
fn waited_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests;
