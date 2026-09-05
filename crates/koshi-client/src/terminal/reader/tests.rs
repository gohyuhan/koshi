//! Tests for filtered terminal-event reads.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};

#[derive(Debug)]
struct Source {
    events: VecDeque<io::Result<Option<Event>>>,
}

impl EventSource for Source {
    fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<Event>> {
        self.events.pop_front().unwrap_or(Ok(None))
    }
}

fn key(character: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(character), Modifiers::NONE))
}

fn reader(events: Vec<io::Result<Option<Event>>>) -> InputReader<Source> {
    InputReader {
        source: Source {
            events: events.into(),
        },
        buffered: VecDeque::new(),
    }
}

#[test]
fn poll_keeps_rejected_events_in_source_order() {
    let mut reader = reader(vec![Ok(Some(key('a'))), Ok(Some(Event::FocusIn))]);
    assert!(reader
        .poll(Some(Duration::from_millis(1)), |event| {
            *event == Event::FocusIn
        })
        .expect("poll succeeds"));
    assert_eq!(reader.read(|_| true).expect("first event"), key('a'));
    assert_eq!(reader.read(|_| true).expect("second event"), Event::FocusIn);
}

#[test]
fn a_timeout_keeps_every_rejected_event() {
    let mut reader = reader(vec![Ok(Some(key('a'))), Ok(None)]);
    assert!(!reader
        .poll(Some(Duration::ZERO), |event| *event == Event::FocusIn)
        .expect("poll succeeds"));
    assert_eq!(reader.read(|_| true).expect("buffered key"), key('a'));
}

#[test]
fn a_source_error_keeps_every_rejected_event() {
    let mut reader = reader(vec![
        Ok(Some(key('a'))),
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
    ]);
    let error = reader
        .poll(None, |event| *event == Event::FocusIn)
        .expect_err("source error is returned");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(reader.read(|_| true).expect("buffered key"), key('a'));
}

#[test]
fn read_can_select_an_event_after_a_rejected_event() {
    let mut reader = reader(vec![Ok(Some(key('a'))), Ok(Some(Event::FocusOut))]);
    assert_eq!(
        reader
            .read(|event| *event == Event::FocusOut)
            .expect("focus event"),
        Event::FocusOut
    );
    assert_eq!(reader.read(|_| true).expect("buffered key"), key('a'));
}
