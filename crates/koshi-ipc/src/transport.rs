//! Framed messages over the local control socket.
//!
//! One running Koshi binds a [`Listener`](crate::transport::Listener); each
//! caller opens a [`Connection`](crate::transport::Connection) to it. On Unix
//! the socket is a Unix domain socket at a filesystem path; on Windows it is
//! a named pipe addressed by bare name (`koshi-…`, which the OS serves as
//! `\\.\pipe\koshi-…`). Both sides speak the same frame shape: a 4-byte
//! big-endian length, then that many bytes of JSON encoding one message from
//! [`protocol`](crate::protocol).
//!
//! A received length prefix is checked against
//! [`MAX_FRAME_LEN`](crate::transport::MAX_FRAME_LEN) before the payload
//! buffer is allocated, so a peer naming a huge length is refused at the cost
//! of reading four bytes.

use std::io::{self, Read, Write};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{self as socket, ListenerOptions};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::IpcError;

/// The largest frame either side sends or accepts: 16 MiB. Every current
/// message is far smaller; the cap bounds what a length prefix can make the
/// reader allocate.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Map a control-socket address — the string an endpoint file stores — to the
/// platform's socket name: a socket-file path on Unix, a pipe name on
/// Windows.
fn socket_name(addr: &str) -> io::Result<socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        addr.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        addr.to_ns_name::<GenericNamespaced>()
    }
}

/// The server end of the control socket: binds the address and accepts one
/// [`Connection`] per caller.
///
/// Dropping the listener releases the address; on Unix the socket file is
/// unlinked.
#[derive(Debug)]
pub struct Listener {
    inner: socket::Listener,
}

impl Listener {
    /// Bind `addr` and start listening. Fails if the address is already
    /// bound, does not fit the platform's socket namespace, or the OS
    /// refuses.
    pub fn bind(addr: &str) -> Result<Listener, IpcError> {
        let name = socket_name(addr).map_err(io_failure)?;
        let inner = ListenerOptions::new()
            .name(name)
            .create_sync()
            .map_err(io_failure)?;
        Ok(Listener { inner })
    }

    /// Block until a caller connects, then hand back that connection.
    ///
    /// On Windows a caller that connects and gives up occupies the pipe until
    /// the next `accept` clears it, so a server calls this in a loop.
    pub fn accept(&self) -> Result<Connection, IpcError> {
        let stream = self.inner.accept().map_err(io_failure)?;
        Ok(Connection { stream })
    }
}

/// One open control-socket connection. Both ends hold one: a caller's comes
/// from [`Connection::connect`], the server's from [`Listener::accept`].
#[derive(Debug)]
pub struct Connection {
    stream: socket::Stream,
}

impl Connection {
    /// Connect to the listener at `addr`. No listener behind the address —
    /// a leftover file whose process is gone, or nothing there at all — is
    /// [`IpcError::NoListener`].
    pub fn connect(addr: &str) -> Result<Connection, IpcError> {
        let name = socket_name(addr).map_err(io_failure)?;
        let stream = socket::Stream::connect(name).map_err(|error| {
            if no_listener_error(&error) {
                IpcError::NoListener {
                    addr: addr.to_string(),
                }
            } else {
                io_failure(error)
            }
        })?;
        Ok(Connection { stream })
    }

    /// Send one message as one frame. Blocks until the bytes are handed to
    /// the OS.
    pub fn send<T: Serialize>(&mut self, message: &T) -> Result<(), IpcError> {
        write_message(&mut self.stream, message)
    }

    /// Read one frame and decode its message as `T`. Blocks until a whole
    /// frame arrives.
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        read_message(&mut self.stream)
    }
}

/// Buffer for one outgoing frame: 4 placeholder length bytes, then the JSON
/// payload as encoding produces it. Refuses the payload byte that crosses
/// [`MAX_FRAME_LEN`], which stops the encoder mid-message, so building a
/// frame never allocates past the cap no matter how large the message is.
struct FrameBuffer {
    /// The frame being built: 4 placeholder bytes, then the payload so far.
    bytes: Vec<u8>,
    /// Set by the refused write: the payload size that write reached.
    overflow: Option<u64>,
}

impl Write for FrameBuffer {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        let reached = self.bytes.len() - 4 + chunk.len();
        if reached > MAX_FRAME_LEN as usize {
            self.overflow = Some(reached as u64);
            return Err(io::Error::other("frame over cap"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode `message` and write it as one frame: 4-byte big-endian length, then
/// the JSON bytes. The whole frame goes out in one `write_all`. A message
/// past [`MAX_FRAME_LEN`] is refused with nothing written, and its encoding
/// stops at the byte that crossed the cap.
fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), IpcError> {
    let mut frame = FrameBuffer {
        bytes: vec![0u8; 4],
        overflow: None,
    };
    if let Err(error) = serde_json::to_writer(&mut frame, message) {
        return Err(match frame.overflow {
            Some(len) => IpcError::FrameTooLarge {
                len,
                max: MAX_FRAME_LEN,
            },
            None => IpcError::MalformedFrame {
                detail: error.to_string(),
            },
        });
    }
    let len = (frame.bytes.len() - 4) as u32;
    frame.bytes[..4].copy_from_slice(&len.to_be_bytes());
    writer.write_all(&frame.bytes).map_err(io_failure)
}

/// Read one frame and decode its JSON payload as `T`. The length prefix is
/// checked against [`MAX_FRAME_LEN`] before the payload buffer is allocated.
fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, IpcError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(io_failure)?;
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len: u64::from(len),
            max: MAX_FRAME_LEN,
        });
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).map_err(io_failure)?;
    serde_json::from_slice(&payload).map_err(|error| IpcError::MalformedFrame {
        detail: error.to_string(),
    })
}

/// True for the connect failures that mean "nothing answers at this
/// address": the connection was refused (a socket file with no listener
/// behind it), nothing exists at the address, or (Unix) the file at the
/// address is not a socket. The errno spellings differ per OS — Linux
/// refuses a non-socket file with `ECONNREFUSED`, macOS with `ENOTSOCK` —
/// so both are checked.
fn no_listener_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ENOTSOCK) {
        return true;
    }
    false
}

/// Classify an IO failure: the kinds that mean "the peer is gone" become
/// [`IpcError::Disconnected`]; everything else keeps its text as
/// [`IpcError::Transport`].
fn io_failure(error: io::Error) -> IpcError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => IpcError::Disconnected,
        _ => IpcError::Transport {
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
