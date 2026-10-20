//! Tests for the Windows console event source on a real console: which input
//! records a read turns into events, and which records it drops without
//! waiting for a key. Each test holds `CONSOLE_INPUT_LOCK` while it uses the
//! console input buffer of the test process. Records typed ahead are written
//! before raw mode, with virtual-terminal input off, and the console stores
//! them as they are. Records written in raw mode pass through the console's
//! virtual-terminal input translation first.

use std::sync::{mpsc, Mutex, MutexGuard, PoisonError};
use std::thread;

use windows_sys::Win32::System::Console::{
    AllocConsole, FlushConsoleInputBuffer, GetNumberOfConsoleInputEvents, WriteConsoleInputW,
    FOCUS_EVENT, FOCUS_EVENT_RECORD, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD,
    KEY_EVENT_RECORD_0,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_F24, VK_RETURN, VK_SHIFT};

use super::*;
use crate::terminal::reader::EventSource as _;

static CONSOLE_INPUT_LOCK: Mutex<()> = Mutex::new(());

/// Hold `CONSOLE_INPUT_LOCK` until the returned guard drops, taking it over
/// from a test that panicked holding it.
fn lock_console_input() -> MutexGuard<'static, ()> {
    CONSOLE_INPUT_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// How long a read that finds no character waits before it reports no event.
const EMPTY_READ_TIMEOUT_DURATION: Duration = Duration::from_millis(200);

/// How long a test waits for a read to return before it reports the read as
/// blocked.
const BLOCKED_READ_LIMIT_DURATION: Duration = Duration::from_secs(5);

/// This test process's console in raw virtual-terminal input mode, holding
/// only `typed_ahead_records`, and the event source that reads it. The records
/// are written in line input mode with virtual-terminal input off. A process
/// with no console gets a new one.
fn open_raw_console(typed_ahead_records: &[INPUT_RECORD]) -> (TerminalDevice, EventSource) {
    // SAFETY: `AllocConsole` takes no arguments. It fails, changing nothing,
    // when the process already has a console.
    unsafe { AllocConsole() };
    let (mut terminal_device, event_source) =
        TerminalDevice::open_terminal_device().expect("open the console");
    terminal_device
        .terminal_input_handle
        .set_console_mode(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)
        .expect("enter line input mode");
    // SAFETY: the handle is the open console input handle.
    let flush_status =
        unsafe { FlushConsoleInputBuffer(terminal_device.terminal_input_handle.get_raw_handle()) };
    assert_ne!(flush_status, 0, "flush the console input buffer");
    if !typed_ahead_records.is_empty() {
        write_input_records(&terminal_device, typed_ahead_records);
    }
    terminal_device
        .enter_raw_mode()
        .expect("enter raw input mode");
    (terminal_device, event_source)
}

/// One key record: `virtual_key_code` pressed or released, carrying
/// `unicode_char`, with `control_key_state`.
fn build_key_record(
    is_key_down: bool,
    virtual_key_code: u16,
    unicode_char: u16,
    control_key_state: u32,
) -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: i32::from(is_key_down),
                wRepeatCount: 1,
                wVirtualKeyCode: virtual_key_code,
                wVirtualScanCode: 0,
                uChar: KEY_EVENT_RECORD_0 {
                    UnicodeChar: unicode_char,
                },
                dwControlKeyState: control_key_state,
            },
        },
    }
}

/// One focus record: the console window gained focus.
fn build_focus_record() -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: FOCUS_EVENT as u16,
        Event: INPUT_RECORD_0 {
            FocusEvent: FOCUS_EVENT_RECORD { bSetFocus: 1 },
        },
    }
}

/// Append `input_records` to the console input buffer of `terminal_device`.
fn write_input_records(terminal_device: &TerminalDevice, input_records: &[INPUT_RECORD]) {
    let mut written_record_count = 0_u32;
    // SAFETY: the pointer and length name `input_records`, and the handle is
    // the open console input handle.
    let write_status = unsafe {
        WriteConsoleInputW(
            terminal_device.terminal_input_handle.get_raw_handle(),
            input_records.as_ptr(),
            u32::try_from(input_records.len()).expect("few records"),
            &mut written_record_count,
        )
    };
    assert_ne!(write_status, 0, "write the console input records");
    assert_eq!(written_record_count as usize, input_records.len());
}

/// The number of records in the console input buffer of `terminal_device`.
fn count_input_records(terminal_device: &TerminalDevice) -> u32 {
    let mut input_record_count = 0_u32;
    // SAFETY: the handle is the open console input handle.
    let count_status = unsafe {
        GetNumberOfConsoleInputEvents(
            terminal_device.terminal_input_handle.get_raw_handle(),
            &mut input_record_count,
        )
    };
    assert_ne!(count_status, 0, "count the console input records");
    input_record_count
}

/// Run one `try_read_event` of `event_source` with `timeout_duration` on its
/// own thread, and return its result and the source. A read still running
/// after `BLOCKED_READ_LIMIT_DURATION` fails the test, after an `x` key record
/// is written that ends the read.
fn read_event_on_thread(
    terminal_device: &TerminalDevice,
    mut event_source: EventSource,
    timeout_duration: Duration,
) -> (io::Result<Option<Event>>, EventSource) {
    let (read_result_sender, read_result_receiver) = mpsc::channel();
    thread::spawn(move || {
        let read_result = event_source.try_read_event(Some(timeout_duration));
        let _ = read_result_sender.send((read_result, event_source));
    });
    match read_result_receiver.recv_timeout(BLOCKED_READ_LIMIT_DURATION) {
        Ok(read_result_and_source) => read_result_and_source,
        Err(_) => {
            write_input_records(
                terminal_device,
                &[build_key_record(true, u16::from(b'X'), u16::from(b'x'), 0)],
            );
            panic!("the read waited for a key past {BLOCKED_READ_LIMIT_DURATION:?}");
        }
    }
}

/// The one event `terminal_bytes` parse to.
fn parse_terminal_bytes(terminal_bytes: &[u8]) -> Event {
    let mut terminal_input_parser = Parser::default();
    terminal_input_parser.process_input_bytes(terminal_bytes);
    terminal_input_parser.finish_pending_input();
    terminal_input_parser
        .remove_next_pending_event()
        .expect("the bytes parse to an event")
}

/// The control key state that holds exactly the modifiers `nul_key_scan`
/// names: Shift `0x100` gives `SHIFT_PRESSED`, Ctrl `0x200` gives
/// `LEFT_CTRL_PRESSED`, and Alt `0x400` gives `LEFT_ALT_PRESSED`. Example:
/// `0x0332` gives `SHIFT_PRESSED | LEFT_CTRL_PRESSED`.
fn build_nul_key_control_key_state(nul_key_scan: u16) -> u32 {
    let nul_key_modifier_bits = nul_key_scan >> 8;
    [
        (1, SHIFT_PRESSED),
        (2, LEFT_CTRL_PRESSED),
        (4, LEFT_ALT_PRESSED),
    ]
    .into_iter()
    .filter(|(modifier_bit, _)| nul_key_modifier_bits & modifier_bit != 0)
    .map(|(_, control_key_flag)| control_key_flag)
    .sum()
}

/// One press of a key that types `unicode_unit`, as a program that types text
/// writes it: virtual key `0`, no modifier.
fn build_typed_unit_record(unicode_unit: u16) -> INPUT_RECORD {
    build_key_record(true, 0, unicode_unit, 0)
}

#[test]
fn typed_ahead_records_that_give_no_character_are_dropped_and_the_read_waits_out_its_timeout() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) = open_raw_console(&[
        build_focus_record(),
        build_key_record(true, VK_SHIFT, 0, SHIFT_PRESSED),
        build_key_record(false, VK_SHIFT, 0, 0),
        build_key_record(false, VK_RETURN, u16::from(b'\r'), 0),
    ]);

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, EMPTY_READ_TIMEOUT_DURATION);

    assert_eq!(read_result.expect("the read succeeds"), None);
    assert_eq!(count_input_records(&terminal_device), 0);
}

#[test]
fn a_focus_record_in_raw_mode_is_dropped_and_the_read_waits_out_its_timeout() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) = open_raw_console(&[]);
    write_input_records(&terminal_device, &[build_focus_record()]);

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, EMPTY_READ_TIMEOUT_DURATION);

    assert_eq!(read_result.expect("the read succeeds"), None);
    assert_eq!(count_input_records(&terminal_device), 0);
}

#[test]
fn a_key_record_behind_dropped_records_reads_as_its_key_and_its_release_reads_nothing() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) = open_raw_console(&[
        build_focus_record(),
        build_key_record(true, VK_SHIFT, 0, SHIFT_PRESSED),
        build_key_record(true, u16::from(b'A'), u16::from(b'a'), 0),
        build_key_record(false, u16::from(b'A'), u16::from(b'a'), 0),
        build_key_record(false, VK_SHIFT, 0, 0),
    ]);

    let (first_read_result, event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);
    let (second_read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, EMPTY_READ_TIMEOUT_DURATION);

    assert_eq!(
        first_read_result.expect("the first read succeeds"),
        Some(parse_terminal_bytes(b"a"))
    );
    assert_eq!(second_read_result.expect("the second read succeeds"), None);
    assert_eq!(count_input_records(&terminal_device), 0);
}

#[test]
fn a_character_on_a_typed_ahead_alt_release_reads_as_that_character() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) =
        open_raw_console(&[build_key_record(false, VK_MENU, 0xE9, 0)]);

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);

    assert_eq!(
        read_result.expect("the read succeeds"),
        Some(parse_terminal_bytes("\u{e9}".as_bytes()))
    );
}

#[test]
fn a_typed_ahead_press_of_the_key_that_types_nul_reads_as_a_nul_byte() {
    let _console_input_guard = lock_console_input();
    // SAFETY: `VkKeyScanW` reads the keyboard layout of this thread.
    let nul_key_scan = unsafe { VkKeyScanW(0) } as u16;
    let (terminal_device, event_source) = open_raw_console(&[build_key_record(
        true,
        nul_key_scan & 0xFF,
        0,
        build_nul_key_control_key_state(nul_key_scan),
    )]);

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);

    assert_eq!(
        read_result.expect("the read succeeds"),
        Some(parse_terminal_bytes(&[0]))
    );
}

/// The records carry no character: a release of the key that types NUL, a
/// press of F24 with the NUL key's modifiers, and three presses of the NUL key
/// with one of Shift, Ctrl and Alt switched from the state that types NUL.
#[test]
fn typed_ahead_records_that_differ_from_the_press_that_types_nul_are_dropped() {
    let _console_input_guard = lock_console_input();
    // SAFETY: `VkKeyScanW` reads the keyboard layout of this thread.
    let nul_key_scan = unsafe { VkKeyScanW(0) } as u16;
    let nul_virtual_key_code = nul_key_scan & 0xFF;
    let nul_control_key_state = build_nul_key_control_key_state(nul_key_scan);
    let (terminal_device, event_source) = open_raw_console(&[
        build_key_record(false, nul_virtual_key_code, 0, nul_control_key_state),
        build_key_record(true, VK_F24, 0, nul_control_key_state),
        build_key_record(
            true,
            nul_virtual_key_code,
            0,
            nul_control_key_state ^ SHIFT_PRESSED,
        ),
        build_key_record(
            true,
            nul_virtual_key_code,
            0,
            nul_control_key_state ^ LEFT_CTRL_PRESSED,
        ),
        build_key_record(
            true,
            nul_virtual_key_code,
            0,
            nul_control_key_state ^ LEFT_ALT_PRESSED,
        ),
    ]);

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, EMPTY_READ_TIMEOUT_DURATION);

    assert_eq!(read_result.expect("the read succeeds"), None);
    assert_eq!(count_input_records(&terminal_device), 0);
}

#[test]
fn a_typed_surrogate_pair_reads_as_one_character() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) = open_raw_console(&[]);
    write_input_records(
        &terminal_device,
        &[
            build_typed_unit_record(0xD83D),
            build_typed_unit_record(0xDE00),
        ],
    );

    let (read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);

    assert_eq!(
        read_result.expect("the read succeeds"),
        Some(parse_terminal_bytes("\u{1F600}".as_bytes()))
    );
}

#[test]
fn a_surrogate_pair_that_one_read_splits_reads_as_one_character() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, mut event_source) = open_raw_console(&[]);
    let mut typed_records =
        vec![build_typed_unit_record(u16::from(b'a')); TERMINAL_INPUT_UNIT_COUNT - 1];
    typed_records.extend([
        build_typed_unit_record(0xD83D),
        build_typed_unit_record(0xDE00),
    ]);
    write_input_records(&terminal_device, &typed_records);

    let mut read_events = Vec::new();
    for _ in 0..TERMINAL_INPUT_UNIT_COUNT {
        let (read_result, returned_event_source) =
            read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);
        event_source = returned_event_source;
        read_events.push(read_result.expect("the read succeeds"));
    }

    let mut expected_events = vec![Some(parse_terminal_bytes(b"a")); TERMINAL_INPUT_UNIT_COUNT - 1];
    expected_events.push(Some(parse_terminal_bytes("\u{1F600}".as_bytes())));
    assert_eq!(read_events, expected_events);
    assert_eq!(count_input_records(&terminal_device), 0);
}

#[test]
fn a_typed_ahead_unpaired_high_surrogate_reads_as_the_replacement_character() {
    let _console_input_guard = lock_console_input();
    let (terminal_device, event_source) = open_raw_console(&[
        build_typed_unit_record(0xD83D),
        build_typed_unit_record(u16::from(b'a')),
    ]);

    let (first_read_result, event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);
    let (second_read_result, _event_source) =
        read_event_on_thread(&terminal_device, event_source, BLOCKED_READ_LIMIT_DURATION);

    assert_eq!(
        first_read_result.expect("the first read succeeds"),
        Some(parse_terminal_bytes("\u{FFFD}".as_bytes()))
    );
    assert_eq!(
        second_read_result.expect("the second read succeeds"),
        Some(parse_terminal_bytes(b"a"))
    );
}
