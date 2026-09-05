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
const ESCAPE_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_BYTES: usize = 4_096;
const OUTPUT_BUFFER_BYTES: usize = 4_096;

/// The mode and output owner for one Windows console.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    input: ConsoleHandle,
    output: BufWriter<ConsoleHandle>,
    original_input_mode: CONSOLE_MODE,
    original_output_mode: CONSOLE_MODE,
    original_input_code_page: u32,
    original_output_code_page: u32,
    raw: bool,
}

impl TerminalDevice {
    /// Open the console and its event source without changing global modes.
    pub(crate) fn open() -> io::Result<(Self, EventSource)> {
        let input = ConsoleHandle::open("CONIN$")?;
        let output = ConsoleHandle::open("CONOUT$")?;
        let source = EventSource::new(input.try_clone()?, output.try_clone()?)?;
        let original_input_mode = input.mode()?;
        let original_output_mode = output.mode()?;
        let original_input_code_page = input_code_page()?;
        let original_output_code_page = output_code_page()?;
        Ok((
            Self {
                input,
                output: BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, output),
                original_input_mode,
                original_output_mode,
                original_input_code_page,
                original_output_code_page,
                raw: false,
            },
            source,
        ))
    }

    /// Enable UTF-8 virtual-terminal input and output with raw key delivery.
    pub(crate) fn enter_raw_mode(&mut self) -> io::Result<()> {
        let input_mode = (self.original_input_mode
            & !(ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_MOUSE_INPUT
                | ENABLE_WINDOW_INPUT))
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let output_mode = self.original_output_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;

        let result = (|| {
            set_input_code_page(CP_UTF8)?;
            set_output_code_page(CP_UTF8)?;
            self.output.get_ref().set_mode(output_mode)?;
            self.input.set_mode(input_mode)
        })();
        if let Err(error) = result {
            let _ = self.restore();
            return Err(error);
        }
        self.raw = true;
        Ok(())
    }

    /// Restore the console modes and code pages captured by [`Self::open`].
    pub(crate) fn enter_cooked_mode(&mut self) -> io::Result<()> {
        if !self.raw {
            return Ok(());
        }
        let result = self.restore();
        if result.is_ok() {
            self.raw = false;
        }
        result
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut first_error = None;
        keep_first(
            &mut first_error,
            self.input.set_mode(self.original_input_mode),
        );
        keep_first(
            &mut first_error,
            self.output.get_ref().set_mode(self.original_output_mode),
        );
        keep_first(
            &mut first_error,
            set_input_code_page(self.original_input_code_page),
        );
        keep_first(
            &mut first_error,
            set_output_code_page(self.original_output_code_page),
        );
        first_error.map_or(Ok(()), Err)
    }
}

impl Write for TerminalDevice {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
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
    input: ConsoleHandle,
    size: ConsoleHandle,
    parser: Parser,
    waker: Arc<EventHandle>,
    pending_since: Option<Instant>,
    last_size: WindowSize,
    next_resize_check: Instant,
}

impl EventSource {
    fn new(input: ConsoleHandle, size: ConsoleHandle) -> io::Result<Self> {
        let last_size = size.window_size()?;
        Ok(Self {
            input,
            size,
            parser: Parser::default(),
            waker: Arc::new(EventHandle::new()?),
            pending_since: None,
            last_size,
            next_resize_check: Instant::now() + RESIZE_POLL_INTERVAL,
        })
    }

    /// Return a handle that interrupts this source's wait.
    pub(crate) fn waker(&self) -> Waker {
        Waker {
            event: Arc::clone(&self.waker),
        }
    }

    fn read_input(&mut self) -> io::Result<()> {
        let mut bytes = [0_u8; INPUT_BYTES];
        let mut count = 0_u32;
        let read = unsafe {
            ReadFile(
                self.input.raw(),
                bytes.as_mut_ptr(),
                INPUT_BYTES as u32,
                &mut count,
                ptr::null_mut(),
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        self.parser.push(&bytes[..count as usize]);
        self.pending_since = self.parser.needs_sequence_timeout().then(Instant::now);
        Ok(())
    }

    fn resize_event(&mut self) -> io::Result<Option<Event>> {
        self.next_resize_check = Instant::now() + RESIZE_POLL_INTERVAL;
        let size = self.size.window_size()?;
        if size == self.last_size {
            return Ok(None);
        }
        self.last_size = size;
        Ok(Some(Event::WindowResized(size)))
    }
}

impl reader::EventSource for EventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            if let Some(event) = self.parser.pop() {
                return Ok(Some(event));
            }
            if Instant::now() >= self.next_resize_check {
                if let Some(event) = self.resize_event()? {
                    return Ok(Some(event));
                }
            }
            let sequence_timeout = self
                .pending_since
                .map(|start| ESCAPE_SEQUENCE_TIMEOUT.saturating_sub(start.elapsed()));
            let wait = shorter(
                Some(
                    self.next_resize_check
                        .saturating_duration_since(Instant::now()),
                ),
                deadline.map(|end| end.saturating_duration_since(Instant::now())),
            );
            let wait = shorter(wait, sequence_timeout);
            let handles = [self.waker.raw(), self.input.raw()];
            let result = unsafe {
                WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    wait.map(wait_millis).unwrap_or(INFINITE),
                )
            };
            match result {
                WAIT_OBJECT_0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "terminal input was interrupted",
                    ));
                }
                value if value == WAIT_OBJECT_0 + 1 => {
                    self.read_input()?;
                    continue;
                }
                WAIT_TIMEOUT => {}
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                value => {
                    return Err(io::Error::other(format!(
                        "unexpected terminal wait result {value}"
                    )))
                }
            }
            if self
                .pending_since
                .is_some_and(|start| start.elapsed() >= ESCAPE_SEQUENCE_TIMEOUT)
            {
                self.parser.finish_pending();
                self.pending_since = None;
                continue;
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(None);
            }
        }
    }
}

/// A cloneable interruption handle for one Windows input source.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    event: Arc<EventHandle>,
}

impl Waker {
    /// Interrupt a blocked event read.
    pub(crate) fn wake(&self) -> io::Result<()> {
        if unsafe { SetEvent(self.event.raw()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct EventHandle(OwnedHandle);

impl EventHandle {
    fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

#[derive(Debug)]
struct ConsoleHandle(OwnedHandle);

impl ConsoleHandle {
    fn open(name: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(name)?;
        Ok(Self(OwnedHandle::from(file)))
    }

    fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    fn mode(&self) -> io::Result<CONSOLE_MODE> {
        let mut mode = 0;
        if unsafe { GetConsoleMode(self.raw(), &mut mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(mode)
        }
    }

    fn set_mode(&self, mode: CONSOLE_MODE) -> io::Result<()> {
        if unsafe { SetConsoleMode(self.raw(), mode) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn window_size(&self) -> io::Result<WindowSize> {
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(self.raw(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let cols = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
        let rows = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
        let cols = u16::try_from(cols)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::other("console window has no columns"))?;
        let rows = u16::try_from(rows)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::other("console window has no rows"))?;
        Ok(WindowSize {
            cols,
            rows,
            pixel_width: None,
            pixel_height: None,
        })
    }
}

impl Write for ConsoleHandle {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(u32::MAX as usize);
        let mut written = 0_u32;
        let result = unsafe {
            WriteFile(
                self.raw(),
                bytes.as_ptr(),
                count as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn input_code_page() -> io::Result<u32> {
    let code_page = unsafe { GetConsoleCP() };
    if code_page == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(code_page)
    }
}

fn output_code_page() -> io::Result<u32> {
    let code_page = unsafe { GetConsoleOutputCP() };
    if code_page == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(code_page)
    }
}

fn set_input_code_page(code_page: u32) -> io::Result<()> {
    if unsafe { SetConsoleCP(code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_output_code_page(code_page: u32) -> io::Result<()> {
    if unsafe { SetConsoleOutputCP(code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn keep_first(first: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first.is_none() {
            *first = Some(error);
        }
    }
}

fn shorter(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn wait_millis(duration: Duration) -> u32 {
    let millis = duration
        .as_millis()
        .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0));
    u32::try_from(millis.min(u128::from(INFINITE - 1))).unwrap_or(INFINITE - 1)
}
