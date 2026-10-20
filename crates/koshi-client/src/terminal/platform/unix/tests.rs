//! Tests for Unix event-source waits and terminal-input endings.

use super::*;
use crate::attach::is_signal_ignored;
use crate::attach::tests::set_signal_actions;
use koshi_input::host::{KeyCode, KeyEvent, Modifiers};
use std::os::fd::{FromRawFd, OwnedFd};

fn build_terminal_event_source() -> (EventSource, UnixStream) {
    let (terminal_input_stream, terminal_input_writer) =
        UnixStream::pair().expect("terminal input stream pair");
    let terminal_input_file = File::from(OwnedFd::from(terminal_input_stream));
    let terminal_size_stream = terminal_input_file
        .try_clone()
        .expect("terminal size handle");
    (
        EventSource::from_terminal_streams(terminal_input_file, terminal_size_stream)
            .expect("event source"),
        terminal_input_writer,
    )
}

#[test]
fn a_waker_interrupts_a_blocked_source() {
    let (mut event_source, _terminal_input_writer) = build_terminal_event_source();
    event_source.create_waker().wake().expect("wake write");
    let io_error =
        reader::EventSource::try_read_event(&mut event_source, None).expect_err("interrupted read");
    assert_eq!(io_error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_waker_wins_when_terminal_input_is_also_ready() {
    let (mut event_source, mut terminal_input_writer) = build_terminal_event_source();
    terminal_input_writer
        .write_all(b"x")
        .expect("terminal input");
    event_source.create_waker().wake().expect("wake write");

    let io_error =
        reader::EventSource::try_read_event(&mut event_source, None).expect_err("interrupted read");

    assert_eq!(io_error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn a_standalone_escape_resolves_after_the_sequence_deadline() {
    let (mut event_source, mut terminal_input_writer) = build_terminal_event_source();
    terminal_input_writer
        .write_all(b"\x1b")
        .expect("terminal input");
    let parsed_terminal_event =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(100)))
            .expect("event read");
    assert_eq!(
        parsed_terminal_event,
        Some(Event::Key(KeyEvent::from_key_code_and_modifiers(
            KeyCode::Escape,
            Modifiers::NONE
        )))
    );
}

#[test]
fn an_expired_unterminated_osc_stays_pending_until_its_terminator() {
    let (mut event_source, mut terminal_input_writer) = build_terminal_event_source();
    terminal_input_writer
        .write_all(b"\x1b]0;title")
        .expect("terminal input");
    event_source
        .process_terminal_input_bytes()
        .expect("OSC input");
    assert_eq!(event_source.input_sequence_started_at, None);
    assert!(event_source.terminal_input_parser.has_pending_input());

    event_source.input_sequence_started_at =
        Some(Instant::now() - ESCAPE_SEQUENCE_TIMEOUT_DURATION);
    let parsed_terminal_event =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::ZERO))
            .expect("timer check");
    assert_eq!(parsed_terminal_event, None);
    assert!(event_source.terminal_input_parser.has_pending_input());

    terminal_input_writer
        .write_all(b"\x1b\\x")
        .expect("terminal input after OSC");
    let parsed_terminal_event =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(100)))
            .expect("event after OSC");
    assert_eq!(
        parsed_terminal_event,
        Some(Event::Key(KeyEvent::from_key_code_and_modifiers(
            KeyCode::Char('x'),
            Modifiers::NONE
        )))
    );
}

#[test]
fn control_string_progress_does_not_arm_an_inactivity_deadline() {
    let (mut event_source, mut terminal_input_writer) = build_terminal_event_source();
    terminal_input_writer
        .write_all(b"\x1b]0;")
        .expect("terminal input");
    event_source
        .process_terminal_input_bytes()
        .expect("OSC opening");
    assert_eq!(event_source.input_sequence_started_at, None);
    assert!(event_source.terminal_input_parser.has_pending_input());

    terminal_input_writer
        .write_all(b"title")
        .expect("terminal input");
    event_source
        .process_terminal_input_bytes()
        .expect("OSC body");
    assert_eq!(event_source.input_sequence_started_at, None);
    assert!(event_source.terminal_input_parser.has_pending_input());
}

#[test]
fn closed_terminal_input_returns_end_of_file() {
    let (mut event_source, terminal_input_writer) = build_terminal_event_source();
    drop(terminal_input_writer);
    let io_error =
        reader::EventSource::try_read_event(&mut event_source, Some(Duration::from_millis(10)))
            .expect_err("closed input");
    assert_eq!(io_error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn raw_mode_ignores_the_stop_signals_until_cooked_mode_gives_back_their_actions() {
    // Only this test sets the action of `SIGTSTP`, `SIGTTIN` or `SIGTTOU` in
    // the test process.
    let mut leader_descriptor: libc::c_int = -1;
    let mut follower_descriptor: libc::c_int = -1;
    // SAFETY: `openpty` writes the two descriptors it opens into the two
    // integers. A null name, a null `termios` and a null window size are
    // allowed.
    let open_result = unsafe {
        libc::openpty(
            &mut leader_descriptor,
            &mut follower_descriptor,
            std::ptr::null_mut(),
            std::ptr::null_mut::<libc::termios>(),
            std::ptr::null_mut::<libc::winsize>(),
        )
    };
    assert_eq!(
        open_result,
        0,
        "openpty failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: `openpty` opened both descriptors for this process, and each one
    // is owned here once.
    let (_leader_terminal, follower_terminal) = unsafe {
        (
            OwnedFd::from_raw_fd(leader_descriptor),
            OwnedFd::from_raw_fd(follower_descriptor),
        )
    };
    let terminal_output_file = File::from(
        follower_terminal
            .try_clone()
            .expect("the follower descriptor is duplicated"),
    );
    let mut terminal_device = TerminalDevice {
        original_termios: termios::tcgetattr(&follower_terminal).expect("terminal attributes"),
        terminal_file: File::from(follower_terminal),
        terminal_output_writer: BufWriter::new(terminal_output_file),
        stop_signal_actions_before_raw_mode: None,
    };
    let list_ignored_stop_signals = || {
        STOP_SIGNALS
            .into_iter()
            .filter(|stop_signal| is_signal_ignored(*stop_signal).expect("signal action"))
            .collect::<Vec<libc::c_int>>()
    };
    set_signal_actions(&STOP_SIGNALS, libc::SIG_DFL);

    terminal_device.enter_raw_mode().expect("raw mode");
    terminal_device.enter_raw_mode().expect("raw mode again");
    assert_eq!(
        list_ignored_stop_signals(),
        vec![libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU]
    );

    terminal_device.enter_cooked_mode().expect("cooked mode");
    assert_eq!(list_ignored_stop_signals(), Vec::<libc::c_int>::new());
}

#[test]
fn zero_pixel_dimensions_are_unknown() {
    assert_eq!(filter_nonzero_pixel_dimension(0), None);
    assert_eq!(filter_nonzero_pixel_dimension(1), Some(1));
    assert_eq!(filter_nonzero_pixel_dimension(u16::MAX), Some(u16::MAX));
}
