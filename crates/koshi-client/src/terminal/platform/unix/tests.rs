//! Tests for Unix event-source waits and terminal-input endings.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};
use std::os::fd::OwnedFd;

fn build_event_source() -> (EventSource, UnixStream) {
    let (input_stream, input_writer) = UnixStream::pair().expect("input stream pair");
    let input_file = File::from(OwnedFd::from(input_stream));
    let size_stream = input_file.try_clone().expect("size handle");
    (
        EventSource::from_terminal_streams(input_file, size_stream).expect("event source"),
        input_writer,
    )
}

#[test]
fn a_waker_interrupts_a_blocked_source() {
    let (mut event_source, _input_writer) = build_event_source();
    event_source.create_waker().wake().expect("wake write");
    let io_error =
        reader::EventSource::try_read_event(&mut event_source, None).expect_err("interrupted read");
    assert_eq!(io_error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_waker_wins_when_terminal_input_is_also_ready() {
    let (mut event_source, mut input_writer) = build_event_source();
    input_writer.write_all(b"x").expect("terminal input");
    event_source.create_waker().wake().expect("wake write");

    let io_error =
        reader::EventSource::try_read_event(&mut event_source, None).expect_err("interrupted read");

    assert_eq!(io_error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_standalone_escape_resolves_after_the_sequence_deadline() {
    let (mut event_source, mut input_writer) = build_event_source();
    input_writer.write_all(b"\x1b").expect("terminal input");
    let parsed_event =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(100)))
            .expect("event read");
    assert_eq!(
        parsed_event,
        Some(Event::Key(KeyEvent::from_key_code_and_modifiers(
            KeyCode::Escape,
            Modifiers::NONE
        )))
    );
}

#[test]
fn an_expired_unterminated_osc_stays_pending_until_its_terminator() {
    let (mut event_source, mut input_writer) = build_event_source();
    input_writer
        .write_all(b"\x1b]0;title")
        .expect("terminal input");
    event_source.read_input_bytes().expect("OSC input");
    assert_eq!(event_source.pending_since, None);
    assert!(event_source.parser.has_pending_input());

    event_source.pending_since = Some(Instant::now() - ESCAPE_SEQUENCE_TIMEOUT_DURATION);
    let parsed_event = reader::EventSource::try_read_event(&mut event_source, Some(Duration::ZERO))
        .expect("timer check");
    assert_eq!(parsed_event, None);
    assert!(event_source.parser.has_pending_input());

    input_writer
        .write_all(b"\x1b\\x")
        .expect("terminal input after OSC");
    let parsed_event =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(100)))
            .expect("event after OSC");
    assert_eq!(
        parsed_event,
        Some(Event::Key(KeyEvent::from_key_code_and_modifiers(
            KeyCode::Char('x'),
            Modifiers::NONE
        )))
    );
}

#[test]
fn control_string_progress_does_not_arm_an_inactivity_deadline() {
    let (mut event_source, mut input_writer) = build_event_source();
    input_writer.write_all(b"\x1b]0;").expect("terminal input");
    event_source.read_input_bytes().expect("OSC opening");
    assert_eq!(event_source.pending_since, None);
    assert!(event_source.parser.has_pending_input());

    input_writer.write_all(b"title").expect("terminal input");
    event_source.read_input_bytes().expect("OSC body");
    assert_eq!(event_source.pending_since, None);
    assert!(event_source.parser.has_pending_input());
}

#[test]
fn closed_terminal_input_returns_end_of_file() {
    let (mut event_source, input_writer) = build_event_source();
    drop(input_writer);
    let io_error =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(10)))
            .expect_err("closed input");
    assert_eq!(io_error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn zero_pixel_dimensions_are_unknown() {
    assert_eq!(get_nonzero_dimension(0), None);
    assert_eq!(get_nonzero_dimension(1), Some(1));
    assert_eq!(get_nonzero_dimension(u16::MAX), Some(u16::MAX));
}
