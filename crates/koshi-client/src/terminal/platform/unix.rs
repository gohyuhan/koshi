//! Unix controlling-terminal access.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_input::host::{Event, Parser, WindowSize};
use rustix::termios::{self, Termios};

use crate::terminal::reader;

const ESCAPE_SEQUENCE_TIMEOUT_DURATION: Duration = Duration::from_millis(25);
const OUTPUT_BUFFER_BYTE_COUNT: usize = 4_096;

/// The controlling terminal file and output owner for one Unix terminal.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    terminal_file: File,
    output_writer: BufWriter<File>,
    original_termios: Termios,
    is_raw_mode: bool,
}

impl TerminalDevice {
    /// Open the controlling terminal and its event source.
    pub(crate) fn open_terminal_device() -> io::Result<(Self, EventSource)> {
        let input_stream = terminal_input()?;
        let output_stream = terminal_output()?;
        let terminal_file = input_stream.try_clone()?;
        let size_stream = output_stream.try_clone()?;
        let original_termios = termios::tcgetattr(&terminal_file)?;
        let event_source = EventSource::from_terminal_streams(input_stream, size_stream)?;
        Ok((
            Self {
                terminal_file,
                output_writer: BufWriter::with_capacity(OUTPUT_BUFFER_BYTE_COUNT, output_stream),
                original_termios,
                is_raw_mode: false,
            },
            event_source,
        ))
    }

    /// Apply byte-at-a-time input without echo or signal processing.
    pub(crate) fn enter_raw_mode(&mut self) -> io::Result<()> {
        let mut raw_termios = self.original_termios.clone();
        raw_termios.make_raw();
        termios::tcsetattr(
            &self.terminal_file,
            termios::OptionalActions::Now,
            &raw_termios,
        )?;
        self.is_raw_mode = true;
        Ok(())
    }

    /// Restore the terminal state captured by [`Self::open_terminal_device`].
    pub(crate) fn enter_cooked_mode(&mut self) -> io::Result<()> {
        if self.is_raw_mode {
            termios::tcsetattr(
                &self.terminal_file,
                termios::OptionalActions::Now,
                &self.original_termios,
            )?;
            self.is_raw_mode = false;
        }
        Ok(())
    }
}

impl Write for TerminalDevice {
    fn write(&mut self, terminal_output_bytes: &[u8]) -> io::Result<usize> {
        self.output_writer.write(terminal_output_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output_writer.flush()
    }
}

impl Drop for TerminalDevice {
    fn drop(&mut self) {
        let _ = self.flush();
        let _ = self.enter_cooked_mode();
    }
}

/// Parsed input, window changes, and interruption for one Unix terminal.
#[derive(Debug)]
pub(crate) struct EventSource {
    parser: Parser,
    input_stream: File,
    size_stream: File,
    resize_pipe: UnixStream,
    resize_registration: signal_hook::SigId,
    wake_pipe: UnixStream,
    wake_write: Arc<UnixStream>,
    pending_since: Option<Instant>,
}

impl EventSource {
    fn from_terminal_streams(input_stream: File, size_stream: File) -> io::Result<Self> {
        let (resize_pipe, resize_write) = UnixStream::pair()?;
        let resize_registration =
            signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, resize_write)?;
        resize_pipe.set_nonblocking(true)?;

        let (wake_pipe, wake_write) = UnixStream::pair()?;
        wake_pipe.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;

        Ok(Self {
            parser: Parser::default(),
            input_stream,
            size_stream,
            resize_pipe,
            resize_registration,
            wake_pipe,
            wake_write: Arc::new(wake_write),
            pending_since: None,
        })
    }

    /// Return a handle that interrupts this source's wait.
    pub(crate) fn create_waker(&self) -> Waker {
        Waker {
            write: Arc::clone(&self.wake_write),
        }
    }

    fn pop_parsed_event(&mut self) -> Option<Event> {
        let parsed_event = self.parser.remove_next_pending_event();
        if !self.parser.needs_input_sequence_timeout() {
            self.pending_since = None;
        }
        parsed_event
    }

    fn read_input_bytes(&mut self) -> io::Result<()> {
        let mut input_bytes = [0_u8; 4_096];
        let byte_count = read_with_interrupt_retry(&mut self.input_stream, &mut input_bytes)?;
        if byte_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        self.parser.process_input_bytes(&input_bytes[..byte_count]);
        self.pending_since = self
            .parser
            .needs_input_sequence_timeout()
            .then(Instant::now);
        Ok(())
    }

    fn read_resize_event(&self) -> io::Result<Event> {
        Ok(Event::WindowResized(read_terminal_window_size(
            &self.size_stream,
        )?))
    }
}

#[cfg(test)]
mod tests;

impl reader::EventSource for EventSource {
    fn try_read_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        let deadline_instant = timeout.map(|timeout_duration| Instant::now() + timeout_duration);
        loop {
            if let Some(parsed_event) = self.pop_parsed_event() {
                return Ok(Some(parsed_event));
            }

            let sequence_timeout = self
                .pending_since
                .map(|start| ESCAPE_SEQUENCE_TIMEOUT_DURATION.saturating_sub(start.elapsed()));
            let wait_timeout = choose_shorter_duration(
                deadline_instant.map(|deadline| deadline.saturating_duration_since(Instant::now())),
                sequence_timeout,
            );
            let [input_ready, resize_ready, wake_ready] = wait_for_file_descriptors(
                [
                    self.input_stream.as_fd(),
                    self.resize_pipe.as_fd(),
                    self.wake_pipe.as_fd(),
                ],
                wait_timeout,
            )?;

            if wake_ready {
                drain_pipe(&self.wake_pipe)?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal input was interrupted",
                ));
            }
            if resize_ready {
                drain_pipe(&self.resize_pipe)?;
                return self.read_resize_event().map(Some);
            }
            if input_ready {
                self.read_input_bytes()?;
                continue;
            }
            if self
                .pending_since
                .is_some_and(|start| start.elapsed() >= ESCAPE_SEQUENCE_TIMEOUT_DURATION)
            {
                self.parser.finish_pending_input();
                self.pending_since = None;
                continue;
            }
            if deadline_instant.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
        }
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.resize_registration);
    }
}

/// A cloneable interruption handle for one Unix input source.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    write: Arc<UnixStream>,
}

impl Waker {
    /// Interrupt a blocked event read.
    pub(crate) fn wake(&self) -> io::Result<()> {
        match (&*self.write).write(&[1]) {
            Ok(_) => Ok(()),
            Err(wake_error) if wake_error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(wake_error) => Err(wake_error),
        }
    }
}

fn terminal_input() -> io::Result<File> {
    if io::stdin().is_terminal() {
        duplicate_file(rustix::stdio::stdin())
    } else {
        open_controlling_terminal()
    }
}

/// Read the current window size of the terminal that receives rendered frames.
///
/// The pixel fields are `None` when the terminal reports them as `0`.
pub(crate) fn read_window_size() -> io::Result<WindowSize> {
    read_terminal_window_size(&terminal_output()?)
}

/// Read one terminal's window size through `TIOCGWINSZ`.
fn read_terminal_window_size(terminal_file: &File) -> io::Result<WindowSize> {
    let terminal_window_size = termios::tcgetwinsize(terminal_file)?;
    Ok(WindowSize {
        column_count: terminal_window_size.ws_col,
        row_count: terminal_window_size.ws_row,
        pixel_width: get_nonzero_dimension(terminal_window_size.ws_xpixel),
        pixel_height: get_nonzero_dimension(terminal_window_size.ws_ypixel),
    })
}

fn terminal_output() -> io::Result<File> {
    if io::stdout().is_terminal() {
        duplicate_file(rustix::stdio::stdout())
    } else {
        open_controlling_terminal()
    }
}

fn duplicate_file(file_descriptor: BorrowedFd<'static>) -> io::Result<File> {
    let owned_file_descriptor: OwnedFd = rustix::io::dup(file_descriptor)?;
    Ok(File::from(owned_file_descriptor))
}

fn open_controlling_terminal() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

fn read_with_interrupt_retry(
    mut input_reader: impl Read,
    input_bytes: &mut [u8],
) -> io::Result<usize> {
    loop {
        match input_reader.read(input_bytes) {
            Err(io_error) if io_error.kind() == io::ErrorKind::Interrupted => continue,
            input_read_result => return input_read_result,
        }
    }
}

fn drain_pipe(pipe_stream: &UnixStream) -> io::Result<()> {
    let mut pipe_bytes = [0_u8; 64];
    loop {
        match (&*pipe_stream).read(&mut pipe_bytes) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(io_error) if io_error.kind() == io::ErrorKind::Interrupted => {}
            Err(io_error) if io_error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(io_error) => return Err(io_error),
        }
    }
}

fn choose_shorter_duration(
    first_duration: Option<Duration>,
    second_duration: Option<Duration>,
) -> Option<Duration> {
    match (first_duration, second_duration) {
        (Some(first_duration), Some(second_duration)) => Some(first_duration.min(second_duration)),
        (Some(timeout_duration), None) | (None, Some(timeout_duration)) => Some(timeout_duration),
        (None, None) => None,
    }
}

fn get_nonzero_dimension(dimension: u16) -> Option<u16> {
    (dimension != 0).then_some(dimension)
}

#[cfg(not(target_os = "macos"))]
fn wait_for_file_descriptors(
    file_descriptors: [BorrowedFd<'_>; 3],
    timeout: Option<Duration>,
) -> io::Result<[bool; 3]> {
    use rustix::event::{PollFd, PollFlags};

    let deadline_instant = timeout.map(|timeout_duration| Instant::now() + timeout_duration);
    loop {
        let mut poll_fds = [
            PollFd::new(&file_descriptors[0], PollFlags::IN),
            PollFd::new(&file_descriptors[1], PollFlags::IN),
            PollFd::new(&file_descriptors[2], PollFlags::IN),
        ];
        let remaining_timeout =
            deadline_instant.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let remaining_timespec = remaining_timeout
            .map(rustix::event::Timespec::try_from)
            .transpose()
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "poll timeout is too large")
            })?;
        match rustix::event::poll(&mut poll_fds, remaining_timespec.as_ref()) {
            Err(poll_error) if poll_error == rustix::io::Errno::INTR => continue,
            Err(poll_error) => return Err(poll_error.into()),
            Ok(_) => {
                let is_file_descriptor_ready = |poll_file_descriptor: &PollFd<'_>| {
                    poll_file_descriptor
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
                };
                return Ok([
                    is_file_descriptor_ready(&poll_fds[0]),
                    is_file_descriptor_ready(&poll_fds[1]),
                    is_file_descriptor_ready(&poll_fds[2]),
                ]);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_for_file_descriptors(
    file_descriptors: [BorrowedFd<'_>; 3],
    timeout: Option<Duration>,
) -> io::Result<[bool; 3]> {
    let raw_file_descriptors = [
        file_descriptors[0].as_raw_fd(),
        file_descriptors[1].as_raw_fd(),
        file_descriptors[2].as_raw_fd(),
    ];
    if raw_file_descriptors
        .iter()
        .any(|file_descriptor| *file_descriptor < 0 || *file_descriptor >= libc::FD_SETSIZE as i32)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal file descriptor exceeds select capacity",
        ));
    }
    let deadline_instant = timeout.map(|timeout_duration| Instant::now() + timeout_duration);
    loop {
        let mut descriptor_set = unsafe { std::mem::zeroed::<libc::fd_set>() };
        unsafe {
            libc::FD_ZERO(&mut descriptor_set);
            for file_descriptor in raw_file_descriptors {
                libc::FD_SET(file_descriptor, &mut descriptor_set);
            }
        }
        let remaining_timeout =
            deadline_instant.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let mut select_timeout = remaining_timeout.map(|timeout_duration| libc::timeval {
            tv_sec: timeout_duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_usec: timeout_duration.subsec_micros() as libc::suseconds_t,
        });
        let select_result = unsafe {
            libc::select(
                raw_file_descriptors.into_iter().max().unwrap_or(0) + 1,
                &mut descriptor_set,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                select_timeout
                    .as_mut()
                    .map_or(std::ptr::null_mut(), |timeout| timeout),
            )
        };
        if select_result >= 0 {
            return Ok(unsafe {
                [
                    libc::FD_ISSET(raw_file_descriptors[0], &descriptor_set),
                    libc::FD_ISSET(raw_file_descriptors[1], &descriptor_set),
                    libc::FD_ISSET(raw_file_descriptors[2], &descriptor_set),
                ]
            });
        }
        let select_error = io::Error::last_os_error();
        if select_error.kind() != io::ErrorKind::Interrupted {
            return Err(select_error);
        }
    }
}
