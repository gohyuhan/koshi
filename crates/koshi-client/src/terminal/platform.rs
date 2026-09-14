//! Platform terminal mode, output, input, resize, and wake handling.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::{
    read_window_size, EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker,
};
#[cfg(windows)]
pub(super) use windows::{
    read_window_size, EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker,
};
