//! Tests for the rate-bounded byte pump.

use super::*;
use std::net::{TcpListener, TcpStream};

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
