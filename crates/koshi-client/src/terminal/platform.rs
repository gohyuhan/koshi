//! Platform terminal mode, output, input, resize, and wake handling.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::{EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker};
#[cfg(windows)]
pub(super) use windows::{
    EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker,
};
