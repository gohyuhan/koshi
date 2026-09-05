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

const ESCAPE_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
const OUTPUT_BUFFER_BYTES: usize = 4_096;

/// The mode and output owner for one Unix controlling terminal.
#[derive(Debug)]
pub(crate) struct TerminalDevice {
    mode: File,
    output: BufWriter<File>,
    original: Termios,
    raw: bool,
}

impl TerminalDevice {
    /// Open the controlling terminal and its event source.
    pub(crate) fn open() -> io::Result<(Self, EventSource)> {
        let input = terminal_input()?;
        let output = terminal_output()?;
        let mode = input.try_clone()?;
        let size = output.try_clone()?;
        let original = termios::tcgetattr(&mode)?;
        let source = EventSource::new(input, size)?;
        Ok((
            Self {
                mode,
                output: BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, output),
                original,
                raw: false,
            },
            source,
        ))
    }

    /// Apply byte-at-a-time input without echo or signal processing.
    pub(crate) fn enter_raw_mode(&mut self) -> io::Result<()> {
        let mut raw = self.original.clone();
        raw.make_raw();
        termios::tcsetattr(&self.mode, termios::OptionalActions::Now, &raw)?;
        self.raw = true;
        Ok(())
    }

    /// Restore the terminal state captured by [`Self::open`].
    pub(crate) fn enter_cooked_mode(&mut self) -> io::Result<()> {
        if self.raw {
            termios::tcsetattr(&self.mode, termios::OptionalActions::Now, &self.original)?;
            self.raw = false;
        }
        Ok(())
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

/// Parsed input, window changes, and interruption for one Unix terminal.
#[derive(Debug)]
pub(crate) struct EventSource {
    parser: Parser,
    input: File,
    size: File,
    resize_pipe: UnixStream,
    resize_registration: signal_hook::SigId,
    wake_pipe: UnixStream,
    wake_write: Arc<UnixStream>,
    pending_since: Option<Instant>,
}

impl EventSource {
    fn new(input: File, size: File) -> io::Result<Self> {
        let (resize_pipe, resize_write) = UnixStream::pair()?;
        let resize_registration =
            signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, resize_write)?;
        resize_pipe.set_nonblocking(true)?;

        let (wake_pipe, wake_write) = UnixStream::pair()?;
        wake_pipe.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;

        Ok(Self {
            parser: Parser::default(),
            input,
            size,
            resize_pipe,
            resize_registration,
            wake_pipe,
            wake_write: Arc::new(wake_write),
            pending_since: None,
        })
    }

    /// Return a handle that interrupts this source's wait.
    pub(crate) fn waker(&self) -> Waker {
        Waker {
            write: Arc::clone(&self.wake_write),
        }
    }

    fn parsed_event(&mut self) -> Option<Event> {
        let event = self.parser.pop();
        if !self.parser.needs_sequence_timeout() {
            self.pending_since = None;
        }
        event
    }

    fn read_input(&mut self) -> io::Result<()> {
        let mut bytes = [0_u8; 4_096];
        let count = retry_read(&mut self.input, &mut bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input reached end-of-file",
            ));
        }
        self.parser.push(&bytes[..count]);
        self.pending_since = self.parser.needs_sequence_timeout().then(Instant::now);
        Ok(())
    }

    fn resize_event(&self) -> io::Result<Event> {
        let size = termios::tcgetwinsize(&self.size)?;
        Ok(Event::WindowResized(WindowSize {
            cols: size.ws_col,
            rows: size.ws_row,
            pixel_width: nonzero(size.ws_xpixel),
            pixel_height: nonzero(size.ws_ypixel),
        }))
    }
}

#[cfg(test)]
mod tests;

impl reader::EventSource for EventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            if let Some(event) = self.parsed_event() {
                return Ok(Some(event));
            }

            let sequence_timeout = self
                .pending_since
                .map(|start| ESCAPE_SEQUENCE_TIMEOUT.saturating_sub(start.elapsed()));
            let timeout = shorter(
                deadline.map(|end| end.saturating_duration_since(Instant::now())),
                sequence_timeout,
            );
            let [input_ready, resize_ready, wake_ready] = wait(
                [
                    self.input.as_fd(),
                    self.resize_pipe.as_fd(),
                    self.wake_pipe.as_fd(),
                ],
                timeout,
            )?;

            if wake_ready {
                drain(&self.wake_pipe)?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal input was interrupted",
                ));
            }
            if resize_ready {
                drain(&self.resize_pipe)?;
                return self.resize_event().map(Some);
            }
            if input_ready {
                self.read_input()?;
                continue;
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn terminal_input() -> io::Result<File> {
    if io::stdin().is_terminal() {
        duplicate(rustix::stdio::stdin())
    } else {
        open_controlling_terminal()
    }
}

fn terminal_output() -> io::Result<File> {
    if io::stdout().is_terminal() {
        duplicate(rustix::stdio::stdout())
    } else {
        open_controlling_terminal()
    }
}

fn duplicate(fd: BorrowedFd<'static>) -> io::Result<File> {
    let owned: OwnedFd = rustix::io::dup(fd)?;
    Ok(File::from(owned))
}

fn open_controlling_terminal() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

fn retry_read(mut input: impl Read, bytes: &mut [u8]) -> io::Result<usize> {
    loop {
        match input.read(bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn drain(stream: &UnixStream) -> io::Result<()> {
    let mut bytes = [0_u8; 64];
    loop {
        match (&*stream).read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
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

fn nonzero(value: u16) -> Option<u16> {
    (value != 0).then_some(value)
}

#[cfg(not(target_os = "macos"))]
fn wait(fds: [BorrowedFd<'_>; 3], timeout: Option<Duration>) -> io::Result<[bool; 3]> {
    use rustix::event::{PollFd, PollFlags};

    let deadline = timeout.map(|duration| Instant::now() + duration);
    loop {
        let mut poll_fds = [
            PollFd::new(&fds[0], PollFlags::IN),
            PollFd::new(&fds[1], PollFlags::IN),
            PollFd::new(&fds[2], PollFlags::IN),
        ];
        let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
        let remaining = remaining
            .map(rustix::event::Timespec::try_from)
            .transpose()
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "poll timeout is too large")
            })?;
        match rustix::event::poll(&mut poll_fds, remaining.as_ref()) {
            Err(error) if error == rustix::io::Errno::INTR => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let ready = |fd: &PollFd<'_>| {
                    fd.revents()
                        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
                };
                return Ok([
                    ready(&poll_fds[0]),
                    ready(&poll_fds[1]),
                    ready(&poll_fds[2]),
                ]);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn wait(fds: [BorrowedFd<'_>; 3], timeout: Option<Duration>) -> io::Result<[bool; 3]> {
    let raw = [fds[0].as_raw_fd(), fds[1].as_raw_fd(), fds[2].as_raw_fd()];
    if raw
        .iter()
        .any(|fd| *fd < 0 || *fd >= libc::FD_SETSIZE as i32)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal file descriptor exceeds select capacity",
        ));
    }
    let deadline = timeout.map(|duration| Instant::now() + duration);
    loop {
        let mut set = unsafe { std::mem::zeroed::<libc::fd_set>() };
        unsafe {
            libc::FD_ZERO(&mut set);
            for fd in raw {
                libc::FD_SET(fd, &mut set);
            }
        }
        let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
        let mut time = remaining.map(|duration| libc::timeval {
            tv_sec: duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_usec: duration.subsec_micros() as libc::suseconds_t,
        });
        let result = unsafe {
            libc::select(
                raw.into_iter().max().unwrap_or(0) + 1,
                &mut set,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                time.as_mut().map_or(std::ptr::null_mut(), |value| value),
            )
        };
        if result >= 0 {
            return Ok(unsafe {
                [
                    libc::FD_ISSET(raw[0], &set),
                    libc::FD_ISSET(raw[1], &set),
                    libc::FD_ISSET(raw[2], &set),
                ]
            });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
