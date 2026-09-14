//! Tests for the rate-bounded byte pump.

use super::*;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// The bytes one time slice may carry in these tests.
const SLICE_BYTE_COUNT: usize = 1024;

/// The duration one time slice covers in these tests.
const SLICE_DURATION: Duration = Duration::from_millis(10);

/// A connected loopback pair: the stream a test writes into, and the stream the
/// pump reads out of.
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let listener_address = listener
        .local_addr()
        .expect("read the bound listener address");
    let sender = TcpStream::connect(listener_address).expect("connect to the listener");
    let (receiver, _) = listener.accept().expect("accept the connection");
    (sender, receiver)
}

#[test]
fn ten_slices_of_bytes_take_ten_slices_of_time_and_arrive_whole() {
    let (mut source_writer, source_reader) = loopback_pair();
    let (destination_writer, mut destination_reader) = loopback_pair();
    let throttle_input_bytes = vec![7_u8; SLICE_BYTE_COUNT * 10];
    let sent_byte_count = throttle_input_bytes.len();

    let counted_writer = std::thread::spawn(move || {
        source_writer
            .write_all(&throttle_input_bytes)
            .expect("write the throttle bytes");
        source_writer
            .shutdown(std::net::Shutdown::Write)
            .expect("close the writing end");
    });

    let test_start_time = Instant::now();
    let pump = pump_throttled(
        source_reader,
        destination_writer,
        SLICE_BYTE_COUNT,
        SLICE_DURATION,
        Instant::now() + Duration::from_secs(10),
    );

    let mut received_bytes = Vec::new();
    destination_reader
        .read_to_end(&mut received_bytes)
        .expect("read what the pump forwarded");
    let copied_byte_count = pump.join().expect("the pump thread ends");
    let elapsed = test_start_time.elapsed();
    counted_writer.join().expect("the writing thread ends");

    assert_eq!(copied_byte_count, sent_byte_count as u64);
    assert_eq!(received_bytes.len(), sent_byte_count);
    assert_eq!(received_bytes, vec![7_u8; sent_byte_count]);
    assert!(
        elapsed >= SLICE_DURATION * 9,
        "10 slices of bytes crossed in {elapsed:?}, faster than the 9-slice floor"
    );
}

#[test]
fn a_peer_that_never_writes_ends_the_pump_at_its_deadline_with_nothing_copied() {
    let (_source_writer, source_reader) = loopback_pair();
    let (destination_writer, _destination_reader) = loopback_pair();
    source_reader
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set the read timeout");

    let pump = pump_throttled(
        source_reader,
        destination_writer,
        SLICE_BYTE_COUNT,
        SLICE_DURATION,
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
const TEST_DEADLINE_DURATION: Duration = Duration::from_secs(60);

/// A slice short enough that the scripted tests below finish quickly.
const SHORT_TIME_SLICE_DURATION: Duration = Duration::from_millis(1);

/// A scripted reader that answers each [`Read::read`] with the next step of a script,
/// then reports end of stream once the script runs out.
struct ScriptedReader {
    script_steps: std::vec::IntoIter<io::Result<Vec<u8>>>,
}

impl ScriptedReader {
    /// A scripted reader that plays `script_steps` in order.
    fn from_steps(script_steps: Vec<io::Result<Vec<u8>>>) -> Self {
        Self {
            script_steps: script_steps.into_iter(),
        }
    }
}

impl Read for ScriptedReader {
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        match self.script_steps.next() {
            Some(Ok(read_bytes)) => {
                read_buffer[..read_bytes.len()].copy_from_slice(&read_bytes);
                Ok(read_bytes.len())
            }
            Some(Err(read_error)) => Err(read_error),
            None => Ok(0),
        }
    }
}

/// A counted writer that keeps every byte it receives, fails once it reaches
/// `failure_after_write_count` writes, and fails every flush when `should_fail_flush` is set.
struct CountedWriter {
    written_bytes: Arc<Mutex<Vec<u8>>>,
    write_count: usize,
    failure_after_write_count: usize,
    should_fail_flush: bool,
}

impl Write for CountedWriter {
    fn write(&mut self, write_bytes: &[u8]) -> io::Result<usize> {
        if self.write_count >= self.failure_after_write_count {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"));
        }
        self.write_count += 1;
        self.written_bytes
            .lock()
            .unwrap()
            .extend_from_slice(write_bytes);
        Ok(write_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.should_fail_flush {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush refused"));
        }
        Ok(())
    }
}

/// A counted writer that accepts every write and returns the bytes it received.
fn collecting_writer() -> (CountedWriter, Arc<Mutex<Vec<u8>>>) {
    failing_writer(usize::MAX)
}

/// A counted writer that fails on write number `failure_after_write_count + 1` and returns
/// the bytes it received before that write.
fn failing_writer(failure_after_write_count: usize) -> (CountedWriter, Arc<Mutex<Vec<u8>>>) {
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    (
        CountedWriter {
            written_bytes: Arc::clone(&written_bytes),
            write_count: 0,
            failure_after_write_count,
            should_fail_flush: false,
        },
        written_bytes,
    )
}

#[test]
fn a_read_timeout_is_a_pause_so_the_bytes_after_it_still_cross() {
    // Unix reports a read timeout as `WouldBlock`, Windows as `TimedOut`. Both
    // must leave the pump running, so bytes offered afterwards still arrive.
    let scripted_reader = ScriptedReader::from_steps(vec![
        Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Ok(b"one".to_vec()),
        Err(io::Error::from(io::ErrorKind::TimedOut)),
        Ok(b"two".to_vec()),
    ]);
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 6);
    assert_eq!(&*received_bytes.lock().unwrap(), b"onetwo");
}

#[test]
fn a_read_error_that_is_not_a_timeout_ends_the_pump_with_what_it_already_copied() {
    let scripted_reader = ScriptedReader::from_steps(vec![
        Ok(b"kept".to_vec()),
        Err(io::Error::from(io::ErrorKind::ConnectionReset)),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 4);
    assert_eq!(&*received_bytes.lock().unwrap(), b"kept");
}

#[test]
fn a_write_failure_ends_the_pump_and_the_failed_bytes_are_not_counted() {
    let scripted_reader = ScriptedReader::from_steps(vec![
        Ok(b"first".to_vec()),
        Ok(b"second".to_vec()),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (counted_writer, received_bytes) = failing_writer(1);

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    // Only the first write's bytes are counted; the refused write's are not.
    assert_eq!(copied_byte_count, 5);
    assert_eq!(&*received_bytes.lock().unwrap(), b"first");
}

#[test]
fn a_flush_failure_ends_the_pump_and_the_flushed_bytes_are_not_counted() {
    let scripted_reader = ScriptedReader::from_steps(vec![
        Ok(b"written".to_vec()),
        // The pump must never reach this step.
        Ok(b"never".to_vec()),
    ]);
    let (mut counted_writer, received_bytes) = collecting_writer();
    counted_writer.should_fail_flush = true;

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    // The write succeeded before the flush failed: the bytes reached the
    // sink, and the count excludes them.
    assert_eq!(copied_byte_count, 0);
    assert_eq!(&*received_bytes.lock().unwrap(), b"written");
}

#[test]
fn a_deadline_already_passed_ends_the_pump_before_the_first_read() {
    let scripted_reader = ScriptedReader::from_steps(vec![Ok(b"unread".to_vec())]);
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() - Duration::from_secs(1),
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 0);
    assert_eq!(&*received_bytes.lock().unwrap(), b"");
}

#[test]
fn a_zero_byte_slice_ends_the_pump_at_once_with_nothing_copied() {
    let throttle_input_bytes: &'static [u8] = b"never crosses";
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        throttle_input_bytes,
        counted_writer,
        0,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 0);
    assert_eq!(&*received_bytes.lock().unwrap(), b"");
}

#[test]
fn an_empty_source_ends_the_pump_with_nothing_copied() {
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        ScriptedReader::from_steps(Vec::new()),
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 0);
    assert_eq!(&*received_bytes.lock().unwrap(), b"");
}

#[test]
fn a_read_that_fills_the_whole_buffer_is_forwarded_whole() {
    let chunk = vec![9_u8; SLICE_BYTE_COUNT];
    let scripted_reader = ScriptedReader::from_steps(vec![Ok(chunk.clone())]);
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + TEST_DEADLINE_DURATION,
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, SLICE_BYTE_COUNT as u64);
    assert_eq!(*received_bytes.lock().unwrap(), chunk);
}

#[test]
fn only_timeouts_until_the_deadline_end_the_pump_with_nothing_copied() {
    let scripted_reader = ScriptedReader::from_steps(
        std::iter::repeat_with(|| Err(io::Error::from(io::ErrorKind::WouldBlock)))
            .take(1000)
            .collect(),
    );
    let (counted_writer, received_bytes) = collecting_writer();

    let copied_byte_count = pump_throttled(
        scripted_reader,
        counted_writer,
        SLICE_BYTE_COUNT,
        SHORT_TIME_SLICE_DURATION,
        Instant::now() + Duration::from_millis(20),
    )
    .join()
    .expect("the pump thread ends");

    assert_eq!(copied_byte_count, 0);
    assert_eq!(&*received_bytes.lock().unwrap(), b"");
}
