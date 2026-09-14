//! Tests for filtered terminal-event reads.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};

#[derive(Debug)]
struct ProbeEventSource {
    events: VecDeque<io::Result<Option<Event>>>,
}

impl EventSource for ProbeEventSource {
    fn try_read_event(&mut self, _timeout: Option<Duration>) -> io::Result<Option<Event>> {
        self.events.pop_front().unwrap_or(Ok(None))
    }
}

fn build_key_event(character: char) -> Event {
    Event::Key(KeyEvent::from_key_code_and_modifiers(
        KeyCode::Char(character),
        Modifiers::NONE,
    ))
}

fn build_input_reader(events: Vec<io::Result<Option<Event>>>) -> InputReader<ProbeEventSource> {
    InputReader {
        event_source: ProbeEventSource {
            events: events.into(),
        },
        buffered_events: VecDeque::new(),
    }
}

#[test]
fn poll_keeps_rejected_events_in_source_order() {
    let mut input_reader = build_input_reader(vec![
        Ok(Some(build_key_event('a'))),
        Ok(Some(Event::FocusIn)),
    ]);
    assert!(input_reader
        .wait_for_event(Some(Duration::from_millis(1)), |event| {
            *event == Event::FocusIn
        })
        .expect("event wait succeeds"));
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("first event"),
        build_key_event('a')
    );
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("second event"),
        Event::FocusIn
    );
}

#[test]
fn a_timeout_keeps_every_rejected_event() {
    let mut input_reader = build_input_reader(vec![Ok(Some(build_key_event('a'))), Ok(None)]);
    assert!(!input_reader
        .wait_for_event(Some(Duration::ZERO), |event| *event == Event::FocusIn)
        .expect("event wait succeeds"));
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("buffered key"),
        build_key_event('a')
    );
}

#[test]
fn a_source_error_keeps_every_rejected_event() {
    let mut input_reader = build_input_reader(vec![
        Ok(Some(build_key_event('a'))),
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
    ]);
    let io_error = input_reader
        .wait_for_event(None, |event| *event == Event::FocusIn)
        .expect_err("source error is returned");
    assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("buffered key"),
        build_key_event('a')
    );
}

#[test]
fn read_can_select_an_event_after_a_rejected_event() {
    let mut input_reader = build_input_reader(vec![
        Ok(Some(build_key_event('a'))),
        Ok(Some(Event::FocusOut)),
    ]);
    assert_eq!(
        input_reader
            .read_matching_event(|event| *event == Event::FocusOut)
            .expect("focus event"),
        Event::FocusOut
    );
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("buffered key"),
        build_key_event('a')
    );
}
