//! Tests for filtered terminal-event reads.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};

impl<Source: EventSource> InputReader<Source> {
    /// A reader over `event_source` with nothing buffered.
    pub(in crate::terminal) fn from_event_source_for_tests(event_source: Source) -> Self {
        Self {
            event_source,
            buffered_terminal_events: VecDeque::with_capacity(32),
        }
    }
}

#[derive(Debug)]
struct ProbeEventSource {
    terminal_event_read_results: VecDeque<io::Result<Option<Event>>>,
}

impl EventSource for ProbeEventSource {
    fn try_read_event(&mut self, _timeout_duration: Option<Duration>) -> io::Result<Option<Event>> {
        self.terminal_event_read_results
            .pop_front()
            .unwrap_or(Ok(None))
    }
}

fn build_terminal_key_event(character: char) -> Event {
    Event::Key(KeyEvent::from_key_code_and_modifiers(
        KeyCode::Char(character),
        Modifiers::NONE,
    ))
}

fn build_terminal_event_reader(
    terminal_event_read_results: Vec<io::Result<Option<Event>>>,
) -> InputReader<ProbeEventSource> {
    InputReader {
        event_source: ProbeEventSource {
            terminal_event_read_results: terminal_event_read_results.into(),
        },
        buffered_terminal_events: VecDeque::new(),
    }
}

#[test]
fn wait_for_event_keeps_rejected_events_in_source_order() {
    let mut input_reader = build_terminal_event_reader(vec![
        Ok(Some(build_terminal_key_event('a'))),
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
        build_terminal_key_event('a')
    );
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("second event"),
        Event::FocusIn
    );
}

#[test]
fn wait_for_event_timeout_keeps_rejected_events() {
    let mut input_reader =
        build_terminal_event_reader(vec![Ok(Some(build_terminal_key_event('a'))), Ok(None)]);
    assert!(!input_reader
        .wait_for_event(Some(Duration::ZERO), |event| *event == Event::FocusIn)
        .expect("event wait succeeds"));
    assert_eq!(
        input_reader
            .read_matching_event(|_| true)
            .expect("buffered key"),
        build_terminal_key_event('a')
    );
}

#[test]
fn wait_for_event_source_error_keeps_rejected_events() {
    let mut input_reader = build_terminal_event_reader(vec![
        Ok(Some(build_terminal_key_event('a'))),
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
        build_terminal_key_event('a')
    );
}

#[test]
fn read_matching_event_selects_after_a_rejected_event() {
    let mut input_reader = build_terminal_event_reader(vec![
        Ok(Some(build_terminal_key_event('a'))),
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
        build_terminal_key_event('a')
    );
}
