//! Tests for Unix event-source waits and terminal-input endings.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};
use std::os::fd::OwnedFd;

fn source() -> (EventSource, UnixStream) {
    let (read, write) = UnixStream::pair().expect("input stream pair");
    let read = File::from(OwnedFd::from(read));
    let size = read.try_clone().expect("size handle");
    (EventSource::new(read, size).expect("event source"), write)
}

#[test]
fn a_waker_interrupts_a_blocked_source() {
    let (mut source, _write) = source();
    source.waker().wake().expect("wake write");
    let error = reader::EventSource::try_read(&mut source, None).expect_err("interrupted read");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_waker_wins_when_terminal_input_is_also_ready() {
    let (mut source, mut write) = source();
    write.write_all(b"x").expect("terminal input");
    source.waker().wake().expect("wake write");

    let error = reader::EventSource::try_read(&mut source, None).expect_err("interrupted read");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_standalone_escape_resolves_after_the_sequence_deadline() {
    let (mut source, mut write) = source();
    write.write_all(b"\x1b").expect("terminal input");
    let event = reader::EventSource::try_read(&mut source, Some(Duration::from_millis(100)))
        .expect("event read");
    assert_eq!(
        event,
        Some(Event::Key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE)))
    );
}

#[test]
fn an_expired_unterminated_osc_releases_the_next_key() {
    let (mut source, mut write) = source();
    write.write_all(b"\x1b]0;title").expect("terminal input");
    let before_read = Instant::now();
    source.read_input().expect("OSC input");
    let after_read = Instant::now();
    let scheduled = source
        .pending_since
        .expect("unterminated OSC schedules recovery");
    assert!((before_read..=after_read).contains(&scheduled));

    source.pending_since = Some(Instant::now() - ESCAPE_SEQUENCE_TIMEOUT);
    let event =
        reader::EventSource::try_read(&mut source, Some(Duration::ZERO)).expect("timeout recovery");
    assert_eq!(event, None);

    write.write_all(b"x").expect("terminal input after OSC");
    let event = reader::EventSource::try_read(&mut source, Some(Duration::from_millis(100)))
        .expect("event after OSC");
    assert_eq!(
        event,
        Some(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            Modifiers::NONE
        )))
    );
}

#[test]
fn control_string_progress_refreshes_its_inactivity_deadline() {
    let (mut source, mut write) = source();
    write.write_all(b"\x1b]0;").expect("terminal input");
    source.read_input().expect("OSC opening");
    let expired = Instant::now() - ESCAPE_SEQUENCE_TIMEOUT;
    source.pending_since = Some(expired);

    write.write_all(b"title").expect("terminal input");
    let before_read = Instant::now();
    source.read_input().expect("OSC body");
    let after_read = Instant::now();

    let refreshed = source
        .pending_since
        .expect("continued OSC keeps a recovery deadline");
    assert!((before_read..=after_read).contains(&refreshed));
}

#[test]
fn closed_terminal_input_returns_end_of_file() {
    let (mut source, write) = source();
    drop(write);
    let error = reader::EventSource::try_read(&mut source, Some(Duration::from_millis(10)))
        .expect_err("closed input");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn zero_pixel_dimensions_are_unknown() {
    assert_eq!(nonzero(0), None);
    assert_eq!(nonzero(1), Some(1));
    assert_eq!(nonzero(u16::MAX), Some(u16::MAX));
}
