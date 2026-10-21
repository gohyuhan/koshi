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

use super::{
    choose_shorter_timeout_duration, ESCAPE_SEQUENCE_TIMEOUT_DURATION, OUTPUT_BUFFER_BYTE_COUNT,
};
use crate::terminal::{limit_window_size, reader};

const TERMINAL_INPUT_BYTE_COUNT: usize = 4_096;

/// The job-control stop signals that raw mode sets to `SIG_IGN`: `SIGTSTP`,
/// `SIGTTIN` and `SIGTTOU`.
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU];

/// The controlling terminal file and output owner for one Unix terminal.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    terminal_file: File,
    terminal_output_writer: BufWriter<File>,
    original_termios: Termios,
    /// `Some` while the terminal is in raw mode: the action each of
    /// [`STOP_SIGNALS`] had before raw mode, in the same order. `None` in
    /// cooked mode.
    stop_signal_actions_before_raw_mode: Option<[libc::sighandler_t; 3]>,
}

impl TerminalDevice {
    /// Open the controlling terminal and its event source.
    pub(crate) fn open_terminal_device() -> io::Result<(Self, EventSource)> {
        let terminal_input_stream = open_terminal_input()?;
        let terminal_output_stream = open_terminal_output()?;
        let terminal_file = terminal_input_stream.try_clone()?;
        let terminal_size_stream = terminal_output_stream.try_clone()?;
        let original_termios = termios::tcgetattr(&terminal_file)?;
        let event_source =
            EventSource::from_terminal_streams(terminal_input_stream, terminal_size_stream)?;
        Ok((
            Self {
                terminal_file,
                terminal_output_writer: BufWriter::with_capacity(
                    OUTPUT_BUFFER_BYTE_COUNT,
                    terminal_output_stream,
                ),
                original_termios,
                stop_signal_actions_before_raw_mode: None,
            },
            event_source,
        ))
    }

    /// Apply byte-at-a-time input without echo or signal processing, then set
    /// each of [`STOP_SIGNALS`] to `SIG_IGN` until [`Self::enter_cooked_mode`].
    /// A `kill -TSTP` sent to this process while the terminal is in raw mode
    /// changes nothing. A call while the terminal is already in raw mode keeps
    /// the actions the first call recorded.
    ///
    /// # Errors
    /// The failure of `tcsetattr`. The stop signals then keep their actions.
    pub(crate) fn enter_raw_mode(&mut self) -> io::Result<()> {
        let mut raw_termios = self.original_termios.clone();
        raw_termios.make_raw();
        termios::tcsetattr(
            &self.terminal_file,
            termios::OptionalActions::Now,
            &raw_termios,
        )?;
        self.stop_signal_actions_before_raw_mode
            .get_or_insert_with(|| {
                STOP_SIGNALS.map(|stop_signal| {
                    // SAFETY: `SIG_IGN` installs no handler code, and a stop
                    // signal is one whose action can be set.
                    unsafe { libc::signal(stop_signal, libc::SIG_IGN) }
                })
            });
        Ok(())
    }

    /// Restore the terminal state captured by [`Self::open_terminal_device`],
    /// then give each of [`STOP_SIGNALS`] back the action it had before raw
    /// mode. In cooked mode, this does nothing.
    ///
    /// # Errors
    /// The failure of `tcsetattr`. The terminal then stays in raw mode, and the
    /// stop signals stay ignored.
    pub(crate) fn enter_cooked_mode(&mut self) -> io::Result<()> {
        let Some(stop_signal_actions_before_raw_mode) = self.stop_signal_actions_before_raw_mode
        else {
            return Ok(());
        };
        termios::tcsetattr(
            &self.terminal_file,
            termios::OptionalActions::Now,
            &self.original_termios,
        )?;
        for (stop_signal, stop_signal_action) in STOP_SIGNALS
            .into_iter()
            .zip(stop_signal_actions_before_raw_mode)
        {
            // SAFETY: `stop_signal_action` is `SIG_DFL` or `SIG_IGN`: koshi
            // installs no handler for a stop signal, and `execve` resets a
            // caught signal to `SIG_DFL`.
            unsafe { libc::signal(stop_signal, stop_signal_action) };
        }
        self.stop_signal_actions_before_raw_mode = None;
        Ok(())
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

/// Parsed input, window changes, and interruption for one Unix terminal.
#[derive(Debug)]
pub(crate) struct EventSource {
    terminal_input_parser: Parser,
    terminal_input_stream: File,
    terminal_size_stream: File,
    resize_pipe: UnixStream,
    resize_registration: signal_hook::SigId,
    wake_pipe: UnixStream,
    wake_pipe_writer: Arc<UnixStream>,
    input_sequence_started_at: Option<Instant>,
}

impl EventSource {
    fn from_terminal_streams(
        terminal_input_stream: File,
        terminal_size_stream: File,
    ) -> io::Result<Self> {
        let (resize_pipe, resize_pipe_writer) = UnixStream::pair()?;
        let resize_registration = signal_hook::low_level::pipe::register(
            signal_hook::consts::SIGWINCH,
            resize_pipe_writer,
        )?;
        resize_pipe.set_nonblocking(true)?;

        let (wake_pipe, wake_pipe_writer) = UnixStream::pair()?;
        wake_pipe.set_nonblocking(true)?;
        wake_pipe_writer.set_nonblocking(true)?;

        Ok(Self {
            terminal_input_parser: Parser::default(),
            terminal_input_stream,
            terminal_size_stream,
            resize_pipe,
            resize_registration,
            wake_pipe,
            wake_pipe_writer: Arc::new(wake_pipe_writer),
            input_sequence_started_at: None,
        })
    }

    /// Return a handle that interrupts this source's wait.
    pub(crate) fn create_waker(&self) -> Waker {
        Waker {
            wake_pipe_writer: Arc::clone(&self.wake_pipe_writer),
        }
    }

    fn pop_parsed_terminal_event(&mut self) -> Option<Event> {
        let parsed_terminal_event = self.terminal_input_parser.remove_next_pending_event();
        if !self.terminal_input_parser.needs_input_sequence_timeout() {
            self.input_sequence_started_at = None;
        }
        parsed_terminal_event
    }

    fn process_terminal_input_bytes(&mut self) -> io::Result<()> {
        let mut terminal_input_bytes = [0_u8; TERMINAL_INPUT_BYTE_COUNT];
        let terminal_input_byte_count =
            read_with_interrupt_retry(&mut self.terminal_input_stream, &mut terminal_input_bytes)?;
        if terminal_input_byte_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        self.terminal_input_parser
            .process_input_bytes(&terminal_input_bytes[..terminal_input_byte_count]);
        self.input_sequence_started_at = self
            .terminal_input_parser
            .needs_input_sequence_timeout()
            .then(Instant::now);
        Ok(())
    }

    fn build_window_resize_event(&self) -> io::Result<Event> {
        Ok(Event::WindowResized(read_terminal_window_size(
            &self.terminal_size_stream,
        )?))
    }
}

#[cfg(test)]
mod tests;

impl reader::EventSource for EventSource {
    fn try_read_event(&mut self, timeout_duration: Option<Duration>) -> io::Result<Option<Event>> {
        let event_wait_deadline_instant =
            timeout_duration.map(|wait_duration| Instant::now() + wait_duration);
        loop {
            if let Some(parsed_terminal_event) = self.pop_parsed_terminal_event() {
                return Ok(Some(parsed_terminal_event));
            }

            let escape_sequence_timeout_duration =
                self.input_sequence_started_at
                    .map(|input_sequence_started_at| {
                        ESCAPE_SEQUENCE_TIMEOUT_DURATION
                            .saturating_sub(input_sequence_started_at.elapsed())
                    });
            let terminal_poll_wait_duration = choose_shorter_timeout_duration(
                event_wait_deadline_instant.map(|event_wait_deadline_instant| {
                    event_wait_deadline_instant.saturating_duration_since(Instant::now())
                }),
                escape_sequence_timeout_duration,
            );
            let [is_terminal_input_ready, is_resize_pipe_ready, is_wake_pipe_ready] =
                wait_for_file_descriptors(
                    [
                        self.terminal_input_stream.as_fd(),
                        self.resize_pipe.as_fd(),
                        self.wake_pipe.as_fd(),
                    ],
                    terminal_poll_wait_duration,
                )?;

            if is_wake_pipe_ready {
                drain_pipe(&self.wake_pipe)?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal input was interrupted",
                ));
            }
            if is_resize_pipe_ready {
                drain_pipe(&self.resize_pipe)?;
                return self.build_window_resize_event().map(Some);
            }
            if is_terminal_input_ready {
                self.process_terminal_input_bytes()?;
                continue;
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

impl Drop for EventSource {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.resize_registration);
    }
}

/// A cloneable interruption handle for one Unix input source.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    wake_pipe_writer: Arc<UnixStream>,
}

impl Waker {
    /// Interrupt a blocked event read.
    pub(crate) fn wake(&self) -> io::Result<()> {
        match (&*self.wake_pipe_writer).write(&[1]) {
            Ok(_) => Ok(()),
            Err(wake_error) if wake_error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(wake_error) => Err(wake_error),
        }
    }
}

fn open_terminal_input() -> io::Result<File> {
    if io::stdin().is_terminal() {
        duplicate_file_descriptor(rustix::stdio::stdin())
    } else {
        open_controlling_terminal()
    }
}

/// Read the current window size of the terminal that receives rendered frames.
///
/// The pixel fields are `None` when the terminal reports them as `0`.
pub(crate) fn read_window_size() -> io::Result<WindowSize> {
    read_terminal_window_size(&open_terminal_output()?)
}

/// Read one terminal's window size through `TIOCGWINSZ`, cut to the cell
/// limit as [`limit_window_size`] cuts it.
fn read_terminal_window_size(terminal_file: &File) -> io::Result<WindowSize> {
    let terminal_window_size = termios::tcgetwinsize(terminal_file)?;
    Ok(limit_window_size(WindowSize {
        column_count: terminal_window_size.ws_col,
        row_count: terminal_window_size.ws_row,
        pixel_width: filter_nonzero_pixel_dimension(terminal_window_size.ws_xpixel),
        pixel_height: filter_nonzero_pixel_dimension(terminal_window_size.ws_ypixel),
    }))
}

fn open_terminal_output() -> io::Result<File> {
    if io::stdout().is_terminal() {
        duplicate_file_descriptor(rustix::stdio::stdout())
    } else {
        open_controlling_terminal()
    }
}

fn duplicate_file_descriptor(file_descriptor: BorrowedFd<'static>) -> io::Result<File> {
    let owned_file_descriptor: OwnedFd = rustix::io::dup(file_descriptor)?;
    Ok(File::from(owned_file_descriptor))
}

fn open_controlling_terminal() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

fn read_with_interrupt_retry(
    mut terminal_reader: impl Read,
    terminal_input_bytes: &mut [u8],
) -> io::Result<usize> {
    loop {
        match terminal_reader.read(terminal_input_bytes) {
            Err(io_error) if io_error.kind() == io::ErrorKind::Interrupted => continue,
            terminal_input_read_result => return terminal_input_read_result,
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

fn filter_nonzero_pixel_dimension(pixel_dimension: u16) -> Option<u16> {
    (pixel_dimension != 0).then_some(pixel_dimension)
}

#[cfg(not(target_os = "macos"))]
fn wait_for_file_descriptors(
    terminal_event_file_descriptors: [BorrowedFd<'_>; 3],
    timeout_duration: Option<Duration>,
) -> io::Result<[bool; 3]> {
    use rustix::event::{PollFd, PollFlags};

    let poll_deadline_instant =
        timeout_duration.map(|wait_duration| Instant::now() + wait_duration);
    loop {
        let mut terminal_event_poll_set = [
            PollFd::new(&terminal_event_file_descriptors[0], PollFlags::IN),
            PollFd::new(&terminal_event_file_descriptors[1], PollFlags::IN),
            PollFd::new(&terminal_event_file_descriptors[2], PollFlags::IN),
        ];
        let remaining_timeout_duration = poll_deadline_instant.map(|poll_deadline_instant| {
            poll_deadline_instant.saturating_duration_since(Instant::now())
        });
        let remaining_timeout_timespec = remaining_timeout_duration
            .map(rustix::event::Timespec::try_from)
            .transpose()
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "poll timeout is too large")
            })?;
        match rustix::event::poll(
            &mut terminal_event_poll_set,
            remaining_timeout_timespec.as_ref(),
        ) {
            Err(poll_error) if poll_error == rustix::io::Errno::INTR => continue,
            Err(poll_error) => return Err(poll_error.into()),
            Ok(_) => {
                let is_file_descriptor_ready = |poll_file_descriptor: &PollFd<'_>| {
                    poll_file_descriptor
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
                };
                return Ok([
                    is_file_descriptor_ready(&terminal_event_poll_set[0]),
                    is_file_descriptor_ready(&terminal_event_poll_set[1]),
                    is_file_descriptor_ready(&terminal_event_poll_set[2]),
                ]);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_for_file_descriptors(
    terminal_event_file_descriptors: [BorrowedFd<'_>; 3],
    timeout_duration: Option<Duration>,
) -> io::Result<[bool; 3]> {
    let raw_file_descriptors = [
        terminal_event_file_descriptors[0].as_raw_fd(),
        terminal_event_file_descriptors[1].as_raw_fd(),
        terminal_event_file_descriptors[2].as_raw_fd(),
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
    let poll_deadline_instant =
        timeout_duration.map(|wait_duration| Instant::now() + wait_duration);
    loop {
        // SAFETY: an all-zero `fd_set` is a valid empty descriptor set.
        let mut descriptor_set = unsafe { std::mem::zeroed::<libc::fd_set>() };
        // SAFETY: `descriptor_set` is a valid `fd_set`, and every descriptor
        // in `raw_file_descriptors` is in `0..FD_SETSIZE`.
        unsafe {
            libc::FD_ZERO(&mut descriptor_set);
            for file_descriptor in raw_file_descriptors {
                libc::FD_SET(file_descriptor, &mut descriptor_set);
            }
        }
        let remaining_timeout_duration = poll_deadline_instant.map(|poll_deadline_instant| {
            poll_deadline_instant.saturating_duration_since(Instant::now())
        });
        let mut select_timeout = remaining_timeout_duration.map(|timeout_duration| libc::timeval {
            tv_sec: timeout_duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_usec: timeout_duration.subsec_micros() as libc::suseconds_t,
        });
        // SAFETY: `descriptor_set` holds only open descriptors below
        // `FD_SETSIZE`, the first argument is one past the largest of them, and
        // `select_timeout` lives across the call.
        let select_result = unsafe {
            libc::select(
                raw_file_descriptors.into_iter().max().unwrap_or(0) + 1,
                &mut descriptor_set,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                select_timeout
                    .as_mut()
                    .map_or(std::ptr::null_mut(), |select_timeout| select_timeout),
            )
        };
        if select_result >= 0 {
            // SAFETY: every descriptor is in `0..FD_SETSIZE`, and
            // `descriptor_set` is the set `select` filled.
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
