//! Windows console access through virtual-terminal input and output.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_input::host::{Event, Parser, WindowSize};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, PeekConsoleInputW,
    ReadConsoleInputW, ReadConsoleW, SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE,
    CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, INPUT_RECORD, KEY_EVENT,
    LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VkKeyScanW, VK_MENU};

use super::{
    choose_shorter_timeout_duration, ESCAPE_SEQUENCE_TIMEOUT_DURATION, OUTPUT_BUFFER_BYTE_COUNT,
};
use crate::terminal::{limit_window_size, reader};

const CP_UTF8: u32 = 65_001;
const RESIZE_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);
const TERMINAL_INPUT_UNIT_COUNT: usize = 2_048;
const PEEKED_INPUT_RECORD_COUNT: usize = 64;

/// The mode and output owner for one Windows console.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    terminal_input_handle: ConsoleHandle,
    terminal_output_writer: BufWriter<ConsoleHandle>,
    original_input_mode: CONSOLE_MODE,
    original_output_mode: CONSOLE_MODE,
    original_output_code_page: u32,
    is_raw_mode: bool,
}

impl TerminalDevice {
    /// Open the console and its event source without changing global modes.
    pub(crate) fn open_terminal_device() -> io::Result<(Self, EventSource)> {
        let terminal_input_handle = ConsoleHandle::open_console_handle("CONIN$")?;
        let terminal_output_handle = ConsoleHandle::open_console_handle("CONOUT$")?;
        let event_source = EventSource::from_console_handles(
            terminal_input_handle.clone_handle()?,
            terminal_output_handle.clone_handle()?,
        )?;
        let original_input_mode = terminal_input_handle.read_console_mode()?;
        let original_output_mode = terminal_output_handle.read_console_mode()?;
        let original_output_code_page = get_output_code_page()?;
        Ok((
            Self {
                terminal_input_handle,
                terminal_output_writer: BufWriter::with_capacity(
                    OUTPUT_BUFFER_BYTE_COUNT,
                    terminal_output_handle,
                ),
                original_input_mode,
                original_output_mode,
                original_output_code_page,
                is_raw_mode: false,
            },
            event_source,
        ))
    }

    /// Enable virtual-terminal input, UTF-8 virtual-terminal output, and raw
    /// key delivery.
    pub(crate) fn enter_raw_mode(&mut self) -> io::Result<()> {
        let terminal_input_mode = (self.original_input_mode
            & !(ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_MOUSE_INPUT
                | ENABLE_WINDOW_INPUT))
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let terminal_output_mode = self.original_output_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;

        let console_mode_change_result = (|| {
            set_output_code_page(CP_UTF8)?;
            self.terminal_output_writer
                .get_ref()
                .set_console_mode(terminal_output_mode)?;
            self.terminal_input_handle
                .set_console_mode(terminal_input_mode)
        })();
        if let Err(console_mode_change_error) = console_mode_change_result {
            let _ = self.restore_console_modes();
            return Err(console_mode_change_error);
        }
        self.is_raw_mode = true;
        Ok(())
    }

    /// Restore the console modes and the output code page captured by
    /// [`Self::open_terminal_device`].
    pub(crate) fn enter_cooked_mode(&mut self) -> io::Result<()> {
        if !self.is_raw_mode {
            return Ok(());
        }
        let console_restore_result = self.restore_console_modes();
        if console_restore_result.is_ok() {
            self.is_raw_mode = false;
        }
        console_restore_result
    }

    fn restore_console_modes(&mut self) -> io::Result<()> {
        let mut first_console_restore_error = None;
        retain_first_console_restore_error(
            &mut first_console_restore_error,
            self.terminal_input_handle
                .set_console_mode(self.original_input_mode),
        );
        retain_first_console_restore_error(
            &mut first_console_restore_error,
            self.terminal_output_writer
                .get_ref()
                .set_console_mode(self.original_output_mode),
        );
        retain_first_console_restore_error(
            &mut first_console_restore_error,
            set_output_code_page(self.original_output_code_page),
        );
        first_console_restore_error.map_or(Ok(()), Err)
    }
}

impl Write for TerminalDevice {
    fn write(&mut self, terminal_output_bytes: &[u8]) -> io::Result<usize> {
        self.terminal_output_writer.write(terminal_output_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.terminal_output_writer.flush()
    }
}

impl Drop for TerminalDevice {
    fn drop(&mut self) {
        let _ = self.flush();
        let _ = self.enter_cooked_mode();
    }
}

/// Parsed input, resize records, and interruption for one Windows console.
#[derive(Debug)]
pub(crate) struct EventSource {
    terminal_input_handle: ConsoleHandle,
    terminal_window_size_handle: ConsoleHandle,
    terminal_input_parser: Parser,
    terminal_wake_event: Arc<TerminalWakeEventHandle>,
    pending_high_surrogate: Option<u16>,
    input_sequence_started_at: Option<Instant>,
    last_reported_window_size: WindowSize,
    next_resize_check_instant: Instant,
}

impl EventSource {
    fn from_console_handles(
        terminal_input_handle: ConsoleHandle,
        terminal_window_size_handle: ConsoleHandle,
    ) -> io::Result<Self> {
        let last_reported_window_size = terminal_window_size_handle.read_window_size()?;
        Ok(Self {
            terminal_input_handle,
            terminal_window_size_handle,
            terminal_input_parser: Parser::default(),
            terminal_wake_event: Arc::new(TerminalWakeEventHandle::new()?),
            pending_high_surrogate: None,
            input_sequence_started_at: None,
            last_reported_window_size,
            next_resize_check_instant: Instant::now() + RESIZE_POLL_INTERVAL_DURATION,
        })
    }

    /// Return a handle that interrupts this source's wait.
    pub(crate) fn create_waker(&self) -> Waker {
        Waker {
            terminal_wake_event_handle: Arc::clone(&self.terminal_wake_event),
        }
    }

    /// Look at up to [`PEEKED_INPUT_RECORD_COUNT`] records at the front of the
    /// console input buffer. When one of them gives a character to a console
    /// read, read the waiting characters into the parser. Otherwise remove
    /// every record it looked at and read nothing. Example: a focus record and
    /// a menu record are removed, and the next wait sees an empty buffer.
    fn process_console_input(&mut self) -> io::Result<()> {
        let mut input_records = [INPUT_RECORD::default(); PEEKED_INPUT_RECORD_COUNT];
        let mut peeked_record_count = 0_u32;
        // SAFETY: `terminal_input_handle` is an open console input handle.
        // `input_records` holds `PEEKED_INPUT_RECORD_COUNT` writable records, and
        // `peeked_record_count` is a writable `u32`.
        let peek_status = unsafe {
            PeekConsoleInputW(
                self.terminal_input_handle.get_raw_handle(),
                input_records.as_mut_ptr(),
                PEEKED_INPUT_RECORD_COUNT as u32,
                &mut peeked_record_count,
            )
        };
        if peek_status == 0 {
            return Err(io::Error::last_os_error());
        }
        let peeked_records = &input_records[..peeked_record_count as usize];
        if peeked_records.is_empty() {
            return Ok(());
        }
        // SAFETY: `VkKeyScanW` takes the character by value and reads no memory.
        let nul_key_scan = unsafe { VkKeyScanW(0) } as u16;
        if peeked_records
            .iter()
            .any(|input_record| has_input_record_character(input_record, nul_key_scan))
        {
            return self.process_terminal_input_characters();
        }
        let mut removed_record_count = 0_u32;
        // SAFETY: `input_records` holds `PEEKED_INPUT_RECORD_COUNT` writable
        // records, and `peeked_record_count` is at most that count: the console
        // writes at most the requested count, and the slice above checks it.
        // `removed_record_count` is a writable `u32`.
        let remove_status = unsafe {
            ReadConsoleInputW(
                self.terminal_input_handle.get_raw_handle(),
                input_records.as_mut_ptr(),
                peeked_record_count,
                &mut removed_record_count,
            )
        };
        if remove_status == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read up to [`TERMINAL_INPUT_UNIT_COUNT`] UTF-16 units from the console
    /// and hand them to the parser as UTF-8. A high surrogate that ends the
    /// read waits for the unit that starts the next read; an unpaired
    /// surrogate becomes U+FFFD. Example: the units `0xD83D` and `0xDE00`
    /// reach the parser as the bytes `F0 9F 98 80`, even when a read ends
    /// between them.
    fn process_terminal_input_characters(&mut self) -> io::Result<()> {
        let mut terminal_input_units = [0_u16; TERMINAL_INPUT_UNIT_COUNT];
        let mut terminal_input_unit_count = 0_u32;
        // SAFETY: `terminal_input_units` holds `TERMINAL_INPUT_UNIT_COUNT` writable
        // UTF-16 units, and `terminal_input_unit_count` is a writable `u32`.
        // `ReadConsoleW` accepts a NULL input control.
        let terminal_input_read_status = unsafe {
            ReadConsoleW(
                self.terminal_input_handle.get_raw_handle(),
                terminal_input_units.as_mut_ptr().cast(),
                TERMINAL_INPUT_UNIT_COUNT as u32,
                &mut terminal_input_unit_count,
                ptr::null(),
            )
        };
        if terminal_input_read_status == 0 {
            return Err(io::Error::last_os_error());
        }
        if terminal_input_unit_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        let read_units = &terminal_input_units[..terminal_input_unit_count as usize];
        let (decoded_units, trailing_high_surrogate) = match read_units.split_last() {
            Some((&last_unit, leading_units)) if (0xD800..=0xDBFF).contains(&last_unit) => {
                (leading_units, Some(last_unit))
            }
            _ => (read_units, None),
        };
        let terminal_input_text: String = char::decode_utf16(
            self.pending_high_surrogate
                .take()
                .into_iter()
                .chain(decoded_units.iter().copied()),
        )
        .map(|decoded_character| decoded_character.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
        self.pending_high_surrogate = trailing_high_surrogate;
        self.terminal_input_parser
            .process_input_bytes(terminal_input_text.as_bytes());
        self.input_sequence_started_at = self
            .terminal_input_parser
            .needs_input_sequence_timeout()
            .then(Instant::now);
        Ok(())
    }

    fn build_resize_event_if_window_size_changed(&mut self) -> io::Result<Option<Event>> {
        self.next_resize_check_instant = Instant::now() + RESIZE_POLL_INTERVAL_DURATION;
        let current_terminal_window_size = self.terminal_window_size_handle.read_window_size()?;
        if current_terminal_window_size == self.last_reported_window_size {
            return Ok(None);
        }
        self.last_reported_window_size = current_terminal_window_size;
        Ok(Some(Event::WindowResized(current_terminal_window_size)))
    }
}

impl reader::EventSource for EventSource {
    fn try_read_event(&mut self, timeout_duration: Option<Duration>) -> io::Result<Option<Event>> {
        let event_wait_deadline_instant =
            timeout_duration.map(|wait_duration| Instant::now() + wait_duration);
        loop {
            if let Some(parsed_terminal_event) =
                self.terminal_input_parser.remove_next_pending_event()
            {
                return Ok(Some(parsed_terminal_event));
            }
            if Instant::now() >= self.next_resize_check_instant {
                if let Some(terminal_resize_event) =
                    self.build_resize_event_if_window_size_changed()?
                {
                    return Ok(Some(terminal_resize_event));
                }
            }
            let escape_sequence_timeout_duration =
                self.input_sequence_started_at
                    .map(|input_sequence_started_at| {
                        ESCAPE_SEQUENCE_TIMEOUT_DURATION
                            .saturating_sub(input_sequence_started_at.elapsed())
                    });
            let terminal_poll_wait_duration = choose_shorter_timeout_duration(
                Some(
                    self.next_resize_check_instant
                        .saturating_duration_since(Instant::now()),
                ),
                event_wait_deadline_instant.map(|event_wait_deadline_instant| {
                    event_wait_deadline_instant.saturating_duration_since(Instant::now())
                }),
            );
            let terminal_poll_wait_duration = choose_shorter_timeout_duration(
                terminal_poll_wait_duration,
                escape_sequence_timeout_duration,
            );
            let terminal_event_wait_handles = [
                self.terminal_wake_event.get_raw_handle(),
                self.terminal_input_handle.get_raw_handle(),
            ];
            // SAFETY: `terminal_event_wait_handles` holds the two open handles that
            // `self` owns, and its length is the count passed.
            let terminal_wait_result = unsafe {
                WaitForMultipleObjects(
                    terminal_event_wait_handles.len() as u32,
                    terminal_event_wait_handles.as_ptr(),
                    0,
                    terminal_poll_wait_duration
                        .map(convert_wait_timeout_to_milliseconds)
                        .unwrap_or(INFINITE),
                )
            };
            match terminal_wait_result {
                WAIT_OBJECT_0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "terminal input was interrupted",
                    ));
                }
                terminal_wait_result if terminal_wait_result == WAIT_OBJECT_0 + 1 => {
                    self.process_console_input()?;
                    continue;
                }
                WAIT_TIMEOUT => {}
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                unexpected_terminal_wait_result => {
                    return Err(io::Error::other(format!(
                        "unexpected terminal wait result {unexpected_terminal_wait_result}"
                    )))
                }
            }
            if self
                .input_sequence_started_at
                .is_some_and(|input_sequence_started_at| {
                    input_sequence_started_at.elapsed() >= ESCAPE_SEQUENCE_TIMEOUT_DURATION
                })
            {
                self.terminal_input_parser.finish_pending_input();
                self.input_sequence_started_at = None;
                continue;
            }
            if event_wait_deadline_instant.is_some_and(|event_wait_deadline_instant| {
                Instant::now() >= event_wait_deadline_instant
            }) {
                return Ok(None);
            }
        }
    }
}

/// A cloneable interruption handle for one Windows input source.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    terminal_wake_event_handle: Arc<TerminalWakeEventHandle>,
}

impl Waker {
    /// Interrupt a blocked event read.
    pub(crate) fn wake(&self) -> io::Result<()> {
        // SAFETY: the handle is the open event that `terminal_wake_event_handle`
        // owns.
        if unsafe { SetEvent(self.terminal_wake_event_handle.get_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct TerminalWakeEventHandle(OwnedHandle);

impl TerminalWakeEventHandle {
    fn new() -> io::Result<Self> {
        // SAFETY: `CreateEventW` accepts NULL security attributes and a NULL name,
        // and creates an unnamed auto-reset event that starts unsignaled.
        let wake_event_handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if wake_event_handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let owned_wake_event_handle =
            // SAFETY: `wake_event_handle` is the non-NULL handle that `CreateEventW`
            // returned, and nothing else owns it.
            unsafe { OwnedHandle::from_raw_handle(wake_event_handle as RawHandle) };
        Ok(Self(owned_wake_event_handle))
    }

    fn get_raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

/// Read the current window size of the console that receives rendered frames:
/// the column and row count of the visible window of `CONOUT$`, cut by
/// [`limit_window_size`]. The pixel fields are always `None`. A window with no
/// column or no row is an error.
pub(crate) fn read_window_size() -> io::Result<WindowSize> {
    ConsoleHandle::open_console_handle("CONOUT$")?.read_window_size()
}

#[derive(Debug)]
struct ConsoleHandle(OwnedHandle);

impl ConsoleHandle {
    fn open_console_handle(console_device_path: &str) -> io::Result<Self> {
        let console_device_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(console_device_path)?;
        Ok(Self(OwnedHandle::from(console_device_file)))
    }

    fn clone_handle(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    fn get_raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    fn read_console_mode(&self) -> io::Result<CONSOLE_MODE> {
        let mut console_mode = 0;
        // SAFETY: `self` owns the open console handle, and `console_mode` is a
        // writable `u32`.
        if unsafe { GetConsoleMode(self.get_raw_handle(), &mut console_mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(console_mode)
        }
    }

    fn set_console_mode(&self, console_mode: CONSOLE_MODE) -> io::Result<()> {
        // SAFETY: `self` owns the open console handle, and the call reads no
        // memory.
        if unsafe { SetConsoleMode(self.get_raw_handle(), console_mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn read_window_size(&self) -> io::Result<WindowSize> {
        let mut console_screen_buffer_info = CONSOLE_SCREEN_BUFFER_INFO::default();
        // SAFETY: `self` owns the open console handle, and
        // `console_screen_buffer_info` is a writable `CONSOLE_SCREEN_BUFFER_INFO`.
        if unsafe {
            GetConsoleScreenBufferInfo(self.get_raw_handle(), &mut console_screen_buffer_info)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let column_count = i32::from(console_screen_buffer_info.srWindow.Right)
            - i32::from(console_screen_buffer_info.srWindow.Left)
            + 1;
        let row_count = i32::from(console_screen_buffer_info.srWindow.Bottom)
            - i32::from(console_screen_buffer_info.srWindow.Top)
            + 1;
        let column_count = u16::try_from(column_count)
            .ok()
            .filter(|column_count| *column_count != 0)
            .ok_or_else(|| io::Error::other("console window has no columns"))?;
        let row_count = u16::try_from(row_count)
            .ok()
            .filter(|row_count| *row_count != 0)
            .ok_or_else(|| io::Error::other("console window has no rows"))?;
        Ok(limit_window_size(WindowSize {
            column_count,
            row_count,
            pixel_width: None,
            pixel_height: None,
        }))
    }
}

impl Write for ConsoleHandle {
    fn write(&mut self, console_output_bytes: &[u8]) -> io::Result<usize> {
        let console_output_byte_count = console_output_bytes.len().min(u32::MAX as usize);
        let mut written_console_output_byte_count = 0_u32;
        // SAFETY: `self` owns the open console handle, which `OpenOptions` opens
        // without overlapped I/O, so a NULL `OVERLAPPED` is allowed.
        // `console_output_bytes` holds at least `console_output_byte_count`
        // readable bytes, and `written_console_output_byte_count` is a writable
        // `u32`.
        let console_output_write_status = unsafe {
            WriteFile(
                self.get_raw_handle(),
                console_output_bytes.as_ptr(),
                console_output_byte_count as u32,
                &mut written_console_output_byte_count,
                ptr::null_mut(),
            )
        };
        if console_output_write_status == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written_console_output_byte_count as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn get_output_code_page() -> io::Result<u32> {
    // SAFETY: `GetConsoleOutputCP` takes no arguments and reads no memory.
    let output_code_page = unsafe { GetConsoleOutputCP() };
    if output_code_page == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(output_code_page)
    }
}

fn set_output_code_page(output_code_page: u32) -> io::Result<()> {
    // SAFETY: `SetConsoleOutputCP` takes the code page by value and reads no
    // memory.
    if unsafe { SetConsoleOutputCP(output_code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Return whether `input_record` gives a character to a console read in
/// virtual-terminal input mode: a key press that carries a character, a
/// release of Alt that carries one, or a press of the key that types NUL.
/// `nul_key_scan` is `VkKeyScanW(0)`: the virtual key in its low byte, and
/// Shift `0x100`, Ctrl `0x200` and Alt `0x400` in its high byte. Example: a
/// press of `a` carrying `a` gives one; a release of `a`, a press of Shift,
/// and a focus record give none.
fn has_input_record_character(input_record: &INPUT_RECORD, nul_key_scan: u16) -> bool {
    if u32::from(input_record.EventType) != KEY_EVENT {
        return false;
    }
    // SAFETY: `EventType` is `KEY_EVENT`, so `Event` holds a
    // `KEY_EVENT_RECORD`.
    let key_event = unsafe { input_record.Event.KeyEvent };
    // SAFETY: the console writes `uChar` as a UTF-16 unit, and every 16-bit
    // pattern is a valid `u16`.
    let unicode_char = unsafe { key_event.uChar.UnicodeChar };
    let is_key_down = key_event.bKeyDown != 0;
    if unicode_char != 0 {
        return is_key_down || key_event.wVirtualKeyCode == VK_MENU;
    }
    let control_key_state = key_event.dwControlKeyState;
    is_key_down
        && key_event.wVirtualKeyCode == nul_key_scan & 0xFF
        && (control_key_state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0)
            == (nul_key_scan & 0x400 != 0)
        && (control_key_state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0)
            == (nul_key_scan & 0x200 != 0)
        && (control_key_state & SHIFT_PRESSED != 0) == (nul_key_scan & 0x100 != 0)
}

fn retain_first_console_restore_error(
    first_console_restore_error: &mut Option<io::Error>,
    console_restore_result: io::Result<()>,
) {
    if let Err(console_restore_error) = console_restore_result {
        if first_console_restore_error.is_none() {
            *first_console_restore_error = Some(console_restore_error);
        }
    }
}

fn convert_wait_timeout_to_milliseconds(timeout_duration: Duration) -> u32 {
    let timeout_millisecond_count = timeout_duration.as_millis().saturating_add(u128::from(
        !timeout_duration.subsec_nanos().is_multiple_of(1_000_000),
    ));
    u32::try_from(timeout_millisecond_count.min(u128::from(INFINITE - 1))).unwrap_or(INFINITE - 1)
}

#[cfg(test)]
mod tests;
