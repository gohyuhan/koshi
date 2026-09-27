//! Windows console access through virtual-terminal input and output.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_input::host::{Event, Parser, WindowSize};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, SetConsoleCP,
    SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO,
    DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT,
    ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
};

use crate::terminal::reader;

const CP_UTF8: u32 = 65_001;
const ESCAPE_SEQUENCE_TIMEOUT_DURATION: Duration = Duration::from_millis(25);
const RESIZE_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);
const TERMINAL_INPUT_BYTE_COUNT: usize = 4_096;
const OUTPUT_BUFFER_BYTE_COUNT: usize = 4_096;

/// The mode and output owner for one Windows console.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    terminal_input_handle: ConsoleHandle,
    terminal_output_writer: BufWriter<ConsoleHandle>,
    original_input_mode: CONSOLE_MODE,
    original_output_mode: CONSOLE_MODE,
    original_input_code_page: u32,
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
        let original_input_code_page = get_input_code_page()?;
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
                original_input_code_page,
                original_output_code_page,
                is_raw_mode: false,
            },
            event_source,
        ))
    }

    /// Enable UTF-8 virtual-terminal input and output with raw key delivery.
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
            set_input_code_page(CP_UTF8)?;
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

    /// Restore the console modes and code pages captured by [`Self::open_terminal_device`].
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
            set_input_code_page(self.original_input_code_page),
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

    fn process_terminal_input_bytes(&mut self) -> io::Result<()> {
        let mut terminal_input_bytes = [0_u8; TERMINAL_INPUT_BYTE_COUNT];
        let mut terminal_input_byte_count = 0_u32;
        let terminal_input_read_status = unsafe {
            ReadFile(
                self.terminal_input_handle.get_raw_handle(),
                terminal_input_bytes.as_mut_ptr(),
                TERMINAL_INPUT_BYTE_COUNT as u32,
                &mut terminal_input_byte_count,
                ptr::null_mut(),
            )
        };
        if terminal_input_read_status == 0 {
            return Err(io::Error::last_os_error());
        }
        if terminal_input_byte_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        self.terminal_input_parser
            .process_input_bytes(&terminal_input_bytes[..terminal_input_byte_count as usize]);
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
                    self.process_terminal_input_bytes()?;
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
        let wake_event_handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if wake_event_handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let owned_wake_event_handle =
            unsafe { OwnedHandle::from_raw_handle(wake_event_handle as RawHandle) };
        Ok(Self(owned_wake_event_handle))
    }

    fn get_raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

/// Read the current window size of the console that receives rendered frames.
///
/// The pixel fields are always `None`: the Windows console reports no pixel
/// dimensions.
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
        if unsafe { GetConsoleMode(self.get_raw_handle(), &mut console_mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(console_mode)
        }
    }

    fn set_console_mode(&self, console_mode: CONSOLE_MODE) -> io::Result<()> {
        if unsafe { SetConsoleMode(self.get_raw_handle(), console_mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn read_window_size(&self) -> io::Result<WindowSize> {
        let mut console_screen_buffer_info = CONSOLE_SCREEN_BUFFER_INFO::default();
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
        Ok(WindowSize {
            column_count,
            row_count,
            pixel_width: None,
            pixel_height: None,
        })
    }
}

impl Write for ConsoleHandle {
    fn write(&mut self, console_output_bytes: &[u8]) -> io::Result<usize> {
        let console_output_byte_count = console_output_bytes.len().min(u32::MAX as usize);
        let mut written_console_output_byte_count = 0_u32;
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

fn get_input_code_page() -> io::Result<u32> {
    let input_code_page = unsafe { GetConsoleCP() };
    if input_code_page == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(input_code_page)
    }
}

fn get_output_code_page() -> io::Result<u32> {
    let output_code_page = unsafe { GetConsoleOutputCP() };
    if output_code_page == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(output_code_page)
    }
}

fn set_input_code_page(input_code_page: u32) -> io::Result<()> {
    if unsafe { SetConsoleCP(input_code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_output_code_page(output_code_page: u32) -> io::Result<()> {
    if unsafe { SetConsoleOutputCP(output_code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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

fn choose_shorter_timeout_duration(
    first_timeout_duration: Option<Duration>,
    second_timeout_duration: Option<Duration>,
) -> Option<Duration> {
    match (first_timeout_duration, second_timeout_duration) {
        (Some(first_timeout_duration), Some(second_timeout_duration)) => {
            Some(first_timeout_duration.min(second_timeout_duration))
        }
        (Some(timeout_duration), None) | (None, Some(timeout_duration)) => Some(timeout_duration),
        (None, None) => None,
    }
}

fn convert_wait_timeout_to_milliseconds(timeout_duration: Duration) -> u32 {
    let timeout_millisecond_count = timeout_duration
        .as_millis()
        .saturating_add(u128::from(timeout_duration.subsec_nanos() % 1_000_000 != 0));
    u32::try_from(timeout_millisecond_count.min(u128::from(INFINITE - 1))).unwrap_or(INFINITE - 1)
}
