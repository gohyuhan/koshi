//! Tests for the rate-bounded byte pump.

use super::*;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// The bytes one slice may carry in these tests.
const SLICE_BYTES: usize = 1024;

/// The span one slice covers in these tests.
const SLICE: Duration = Duration::from_millis(10);

/// A connected loopback pair: the stream a test writes into, and the stream the
/// pump reads out of.
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let sender = TcpStream::connect(address).expect("connect to the listener");
    let (receiver, _) = listener.accept().expect("accept the connection");
    (sender, receiver)
}

#[test]
fn ten_slices_of_bytes_take_ten_slices_of_time_and_arrive_whole() {
    let (mut source_in, source_out) = loopback_pair();
    let (sink_in, mut sink_out) = loopback_pair();
    let payload = vec![7_u8; SLICE_BYTES * 10];
    let sent = payload.len();

    let writer = std::thread::spawn(move || {
        source_in.write_all(&payload).expect("write the payload");
        source_in
            .shutdown(std::net::Shutdown::Write)
            .expect("close the writing end");
    });

    let started = Instant::now();
    let pump = pump_throttled(
        source_out,
        sink_in,
        SLICE_BYTES,
        SLICE,
        Instant::now() + Duration::from_secs(10),
    );

    let mut arrived = Vec::new();
    sink_out
        .read_to_end(&mut arrived)
        .expect("read what the pump forwarded");
    let copied = pump.join().expect("the pump thread ends");
    let elapsed = started.elapsed();
    writer.join().expect("the writing thread ends");

    assert_eq!(copied, sent as u64);
    assert_eq!(arrived.len(), sent);
    assert_eq!(arrived, vec![7_u8; sent]);
    assert!(
        elapsed >= SLICE * 9,
        "10 slices of bytes crossed in {elapsed:?}, faster than the 9-slice floor"
    );
}

#[test]
fn a_peer_that_never_writes_ends_the_pump_at_its_deadline_with_nothing_copied() {
    let (_source_in, source_out) = loopback_pair();
    let (sink_in, _sink_out) = loopback_pair();
    source_out
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set the read timeout");

    let pump = pump_throttled(
        source_out,
        sink_in,
        SLICE_BYTES,
        SLICE,
        Instant::now() + Duration::from_millis(300),
    );

    let deadline = Instant::now() + Duration::from_secs(1);
    while !pump.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the pump never stopped at its deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pump.join().expect("the pump thread ends"), 0);
}

/// A deadline far past the end of every scripted test below. Each test ends
/// on the stream event it is about, long before this deadline; none asserts
/// on it.
const NO_DEADLINE_REACHED: Duration = Duration::from_secs(60);

/// A slice short enough that the scripted tests below finish quickly.
const SHORT_SLICE: Duration = Duration::from_millis(1);

/// A reader that answers each [`Read::read`] with the next step of a script,
/// then reports end of stream once the script runs out.
struct ScriptedReader {
    steps: std::vec::IntoIter<io::Result<Vec<u8>>>,
}

impl ScriptedReader {
    /// A reader that plays `steps` in order.
    fn new(steps: Vec<io::Result<Vec<u8>>>) -> Self {
        Self {
            steps: steps.into_iter(),
        }
    }
}

impl Read for ScriptedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.steps.next() {
            Some(Ok(bytes)) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Err(error)) => Err(error),
            None => Ok(0),
        }
    }
}

/// A writer that keeps every byte it takes, fails once it has taken
/// `fail_after` writes, and fails every flush when `fail_flush` is set.
struct CountedWriter {
    taken: Arc<Mutex<Vec<u8>>>,
    writes: usize,
    fail_after: usize,
    fail_flush: bool,
}

impl Write for CountedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.writes >= self.fail_after {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"));
        }
        self.writes += 1;
        self.taken.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush refused"));
        }
        Ok(())
    }
}

/// A writer that takes everything, and the buffer holding what it took.
fn collecting_writer() -> (CountedWriter, Arc<Mutex<Vec<u8>>>) {
    failing_writer(usize::MAX)
}

/// A writer that fails on write number `fail_after + 1`, and the buffer holding
/// what it took before that.
fn failing_writer(fail_after: usize) -> (CountedWriter, Arc<Mutex<Vec<u8>>>) {
    let taken = Arc::new(Mutex::new(Vec::new()));
    (
        CountedWriter {
            taken: Arc::clone(&taken),
            writes: 0,
            fail_after,
            fail_flush: false,
        },
        taken,
    )
}

#[test]
fn a_read_timeout_is_a_pause_so_the_bytes_after_it_still_cross() {
    // Unix reports a read timeout as `WouldBlock`, Windows as `TimedOut`. Both
    // must leave the pump running, so bytes offered afterwards still arrive.
    let reader = ScriptedReader::new(vec![
        Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Ok(b"one".to_vec()),
        Err(io::Error::from(io::ErrorKind::TimedOut)),
        Ok(b"two".to_vec()),
    ]);
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 6);
    assert_eq!(&*arrived.lock().unwrap(), b"onetwo");
}

#[test]
fn a_read_error_that_is_not_a_timeout_ends_the_pump_with_what_it_already_copied() {
    let reader = ScriptedReader::new(vec![
        Ok(b"kept".to_vec()),
        Err(io::Error::from(io::ErrorKind::ConnectionReset)),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 4);
    assert_eq!(&*arrived.lock().unwrap(), b"kept");
}

#[test]
fn a_write_failure_ends_the_pump_and_the_failed_bytes_are_not_counted() {
    let reader = ScriptedReader::new(vec![
        Ok(b"first".to_vec()),
        Ok(b"second".to_vec()),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (writer, arrived) = failing_writer(1);

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    // Only the first write's bytes are counted; the refused write's are not.
    assert_eq!(copied, 5);
    assert_eq!(&*arrived.lock().unwrap(), b"first");
}

#[test]
fn a_flush_failure_ends_the_pump_and_the_flushed_bytes_are_not_counted() {
    let reader = ScriptedReader::new(vec![
        Ok(b"written".to_vec()),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (mut writer, arrived) = collecting_writer();
    writer.fail_flush = true;

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    // The write succeeded before the flush failed: the bytes reached the
    // sink, and the count excludes them.
    assert_eq!(copied, 0);
    assert_eq!(&*arrived.lock().unwrap(), b"written");
}

#[test]
fn a_deadline_already_passed_ends_the_pump_before_the_first_read() {
    let reader = ScriptedReader::new(vec![Ok(b"unread".to_vec())]);
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() - Duration::from_secs(1),
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 0);
    assert_eq!(&*arrived.lock().unwrap(), b"");
}

#[test]
fn a_zero_byte_slice_ends_the_pump_at_once_with_nothing_copied() {
    let source: &'static [u8] = b"never crosses";
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        source,
        writer,
        0,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 0);
    assert_eq!(&*arrived.lock().unwrap(), b"");
}

#[test]
fn an_empty_source_ends_the_pump_with_nothing_copied() {
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        ScriptedReader::new(Vec::new()),
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 0);
    assert_eq!(&*arrived.lock().unwrap(), b"");
}

#[test]
fn a_read_that_fills_the_whole_buffer_is_forwarded_whole() {
    let chunk = vec![9_u8; SLICE_BYTES];
    let reader = ScriptedReader::new(vec![Ok(chunk.clone())]);
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + NO_DEADLINE_REACHED,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, SLICE_BYTES as u64);
    assert_eq!(*arrived.lock().unwrap(), chunk);
}

#[test]
fn only_timeouts_until_the_deadline_end_the_pump_with_nothing_copied() {
    let reader = ScriptedReader::new(
        std::iter::repeat_with(|| Err(io::Error::from(io::ErrorKind::WouldBlock)))
            .take(1000)
            .collect(),
    );
    let (writer, arrived) = collecting_writer();

    let copied = pump_throttled(
        reader,
        writer,
        SLICE_BYTES,
        SHORT_SLICE,
        Instant::now() + Duration::from_millis(20),
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied, 0);
    assert_eq!(&*arrived.lock().unwrap(), b"");
}
