//! Tests for filtered terminal-event reads.

use super::*;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};

#[derive(Debug)]
struct ProbeEventSource {
    terminal_event_read_results: VecDeque<io::Result<Option<Event>>>,
}

impl EventSource for ProbeEventSource {
    /// The next scripted result, each event tagged with the move count
    /// `input_read_clock` holds at this call.
    fn try_read_event(
        &mut self,
        _timeout_duration: Option<Duration>,
        input_read_clock: &InputReadClock,
    ) -> io::Result<Option<TaggedEvent>> {
        let terminal_event_read_result = self
            .terminal_event_read_results
            .pop_front()
            .unwrap_or(Ok(None));
        terminal_event_read_result.map(|terminal_event| {
            terminal_event.map(|event| TaggedEvent {
                event,
                input_tag: input_read_clock.get_painted_input_pane_moved_by_others_count(),
            })
        })
    }

    fn remove_next_parsed_event(&mut self) -> Option<TaggedEvent> {
        None
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
    InputReader::from_event_source(
        ProbeEventSource {
            terminal_event_read_results: terminal_event_read_results.into(),
        },
        Arc::default(),
    )
}

#[test]
fn an_event_read_after_a_paint_carries_the_move_count_the_clock_holds_then() {
    let input_read_clock = Arc::new(InputReadClock::default());
    let mut input_reader = InputReader::from_event_source(
        ProbeEventSource {
            terminal_event_read_results: VecDeque::from([
                Ok(Some(build_terminal_key_event('a'))),
                Ok(Some(build_terminal_key_event('b'))),
            ]),
        },
        Arc::clone(&input_read_clock),
    );

    let first_read_events = input_reader
        .read_events_of_next_terminal_read()
        .expect("the first read succeeds");
    input_read_clock.set_painted_input_pane_moved_by_others_count(3);
    let second_read_events = input_reader
        .read_events_of_next_terminal_read()
        .expect("the second read succeeds");

    assert_eq!(
        (first_read_events, second_read_events),
        (
            vec![TaggedEvent {
                event: build_terminal_key_event('a'),
                input_tag: 0,
            }],
            vec![TaggedEvent {
                event: build_terminal_key_event('b'),
                input_tag: 3,
            }],
        )
    );
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
