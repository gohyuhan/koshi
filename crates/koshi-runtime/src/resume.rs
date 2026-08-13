//! The state one session server hands to the process image that replaces it.
//!
//! A session server that replaces its own binary keeps its panes, their child
//! processes and their terminals running, but not its memory. It writes what
//! the next image must take back into one JSON file —
//! `session-<uuid>.resume`, beside the endpoint file.
//!
//! The **header** ([`ResumeHeader`]) names the session and every live pane. Its
//! shape never changes: every field added to it carries `#[serde(default)]`, so
//! a build that cannot read the body still reads the header and can close every
//! descriptor and end every child.
//!
//! The **body** ([`ResumeBody`]) carries the fields that type names. Its shape
//! does change, so
//! [`ResumeHeader::format`] numbers it: [`RESUME_FORMAT`] is what this build
//! writes, [`RESUME_FORMAT_MIN`] the oldest it reads, and [`read_body`] refuses
//! anything outside that range.
//!
//! Example: a server holding two panes writes
//! `{"header":{"format":1,…,"panes":[{"pane_id":…,"pid":51234,"rows":20,"cols":78,"terminal_fd":9,"terminal_name":"/dev/ttys009","exit":null},…]},"body":{…}}`.
//! The next image reads the header, checks that descriptor 9 is still the
//! master of `/dev/ttys009`, takes it and process 51234 back as that pane, then
//! reads the body and puts the pane's screen back under it.

use std::collections::HashMap;
use std::path::Path;

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_session::session::state::Session;
use koshi_storage::error::StorageError;
use koshi_terminal::state::TerminalState;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// The resume-file format this build writes.
pub const RESUME_FORMAT: u32 = 1;

/// The oldest resume-file format this build reads.
pub const RESUME_FORMAT_MIN: u32 = 1;

/// One live pane, as the header names it: what the next image needs to take
/// the pane back, or to shut it down when the body is unreadable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The process id of the pane's child.
    pub pid: u32,
    /// Height in cells of the pane's terminal.
    pub rows: u16,
    /// Width in cells of the pane's terminal.
    pub cols: u16,
    /// The descriptor of the pane's own terminal on Unix. Always `None` on
    /// Windows, where the pseudoconsole stays in the supervisor process and no
    /// descriptor crosses the swap.
    pub terminal_fd: Option<i32>,
    /// The terminal that descriptor was the master of when the state was
    /// carried out, for example `/dev/ttys009`. The next image reads the name of
    /// the descriptor it is handed and takes the pane back only when the two
    /// agree. Always `None` on Windows.
    ///
    /// `None` is also what a header written by a build that recorded no name
    /// carries; the next image then reads the descriptor's kind alone.
    #[serde(default)]
    pub terminal_name: Option<String>,
    /// How the pane's child ended, when the writing process reaped it before it
    /// wrote this file. A reaping process takes the status out of the kernel
    /// with it, so the next image reports this instead of waiting on the
    /// process id.
    ///
    /// `None` says the child was still running and the next image waits on it
    /// itself. It is also what a header written by a build that recorded no
    /// status carries.
    #[serde(default)]
    pub exit: Option<ExitStatus>,
}

impl CarriedPane {
    /// The pane's terminal size, as [`rows`](Self::rows) and
    /// [`cols`](Self::cols) name it.
    #[must_use]
    pub fn size(&self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
        }
    }
}

/// The half of the resume file whose shape never changes: which session this
/// is, and every pane it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeHeader {
    /// Which format the body is written in.
    pub format: u32,
    /// The session the writing process serves.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
    /// Every live pane, in the order the PTY backend reported them.
    pub panes: Vec<CarriedPane>,
}

/// The half of the resume file that [`RESUME_FORMAT`] numbers. Its fields below
/// are what a session server hands the image replacing it.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeBody {
    /// Every session the writing process held, keyed by id. Each one owns its
    /// tabs, layout trees, pane records and attached clients.
    pub sessions: HashMap<SessionId, Session>,
    /// Each pane's screen state, keyed by pane id: grids, scrollback, modes and
    /// cursor. The parser that fed it is not carried; `undecoded` carries that
    /// parser's position.
    pub engines: HashMap<PaneId, TerminalState>,
    /// The bytes that put each pane's next parser where the last one stood,
    /// keyed by pane id, exactly as
    /// [`TerminalEngine::undecoded`](koshi_terminal::engine::TerminalEngine::undecoded)
    /// reports them; a pane it reports nothing for has no entry. The next image
    /// hands an entry to
    /// [`TerminalEngine::from_state`](koshi_terminal::engine::TerminalEngine::from_state).
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(default)]
    pub undecoded: HashMap<PaneId, Vec<u8>>,
    /// A quit that was applied and not yet carried out, and how it must be
    /// carried out.
    ///
    /// A `core:quit` can land after the clients have been told the session is
    /// restarting and are already waiting for its next socket. The swap runs to
    /// the end so they find that socket, and the next image carries the quit
    /// out once every carried client is back or its window has closed — so each
    /// one reads a real quit rather than a session that stopped answering.
    ///
    /// The kind travels with it: a caller that asked for a zero-grace teardown
    /// gets one from the next image too.
    ///
    /// A body whose JSON carries no value for this field reads back as `None`.
    #[serde(default)]
    pub quit: Option<CarriedQuit>,
}

/// How a quit carried across an image swap must be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CarriedQuit {
    /// Each pane's child is asked to stop and given the graceful window before
    /// it is killed.
    Graceful,
    /// Every pane's child is killed at once, with no graceful window.
    Immediate,
}

/// The file as it is read: the header decoded, the body left as the raw JSON
/// text it was written as, so an unreadable body never costs the caller the
/// header. [`read_body`] decodes that text once.
#[derive(Debug, Deserialize)]
struct ResumeFile {
    header: ResumeHeader,
    body: Box<RawValue>,
}

/// The same two halves as [`ResumeFile`], borrowed for the write so no pane's
/// grid or scrollback is copied on its way to the disk.
#[derive(Debug, Serialize)]
struct ResumeFileRef<'a> {
    header: &'a ResumeHeader,
    body: &'a ResumeBody,
}

/// Write `header` and `body` to `path`, replacing whatever is there.
///
/// The bytes land through [`koshi_storage::atomic::write_atomic`], so a reader
/// finds the whole old file or the whole new one, never a half-written middle.
///
/// # Errors
/// Returns [`StorageError::Io`] when the state cannot be encoded, or when the
/// write does not land durably.
pub fn write(path: &Path, header: &ResumeHeader, body: &ResumeBody) -> Result<(), StorageError> {
    let data =
        serde_json::to_vec(&ResumeFileRef { header, body }).map_err(|error| StorageError::Io {
            detail: format!("encode resume state for {}: {error}", path.display()),
        })?;
    koshi_storage::atomic::write_atomic(path, &data)
}

/// Read the resume file at `path`: its header, and its body as raw JSON for
/// [`read_body`].
///
/// The header's shape never changes, so this call answers for a file any build
/// wrote. It does not look at [`ResumeHeader::format`], so a caller holding a
/// body it cannot read still gets every pane's descriptor and process id.
///
/// # Errors
/// Returns [`StorageError::Io`] when the file cannot be read, and
/// [`StorageError::Corrupt`] when its bytes are not a resume file.
pub fn read_header(path: &Path) -> Result<(ResumeHeader, Box<RawValue>), StorageError> {
    let data = std::fs::read(path).map_err(|error| StorageError::Io {
        detail: format!("read resume state at {}: {error}", path.display()),
    })?;
    let file: ResumeFile =
        serde_json::from_slice(&data).map_err(|error| StorageError::Corrupt {
            detail: format!("resume state at {} is unreadable: {error}", path.display()),
        })?;
    Ok((file.header, file.body))
}

/// Decode the raw `body` [`read_header`] handed back, given the `format` the
/// same header named.
///
/// # Errors
/// Returns [`StorageError::Corrupt`] when `format` is outside
/// `RESUME_FORMAT_MIN..=RESUME_FORMAT`, and when the body is not that format's
/// shape.
pub fn read_body(format: u32, body: &RawValue) -> Result<ResumeBody, StorageError> {
    if !(RESUME_FORMAT_MIN..=RESUME_FORMAT).contains(&format) {
        return Err(StorageError::Corrupt {
            detail: format!(
                "resume body format {format} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
            ),
        });
    }
    serde_json::from_str(body.get()).map_err(|error| StorageError::Corrupt {
        detail: format!("resume body is unreadable: {error}"),
    })
}

#[cfg(test)]
mod tests;
