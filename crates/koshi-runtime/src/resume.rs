//! The state one session server hands to the process image that replaces it.
//!
//! A session server that replaces its own binary keeps its panes, their child
//! processes and their terminals running, but not its memory. It writes what
//! the next image must take back into one JSON file —
//! `session-<uuid>.resume`, beside the endpoint file.
//!
//! The **header** ([`ResumeHeader`]) names the session and every live pane.
//! The reader converts headers written by older builds into this shape before
//! it reads the body.
//!
//! The **body** ([`ResumeBody`]) carries the fields that type names. Its shape
//! does change, so
//! [`ResumeHeader::resume_format`] numbers it: [`RESUME_FORMAT`] is what this build
//! writes, [`RESUME_FORMAT_MIN`] the oldest it reads, and [`read_resume_body`] refuses
//! anything outside that range. This build writes format 4 and converts formats
//! 1 through 3 through adjacent migration steps before it restores the body.
//!
//! Each pane's screen is one [`CarriedPaneState`]. For a format 4 body, an
//! unreadable pane state is left out while other panes keep their screens.
//! An unreadable migrated pane makes the whole body unreadable; the session
//! server then restores the running panes with blank screens and separate tabs.
//!
//! Example: a server holding two panes writes
//! `{"header":{"resume_format":4,"session_id":…,"session_name":"quiet-lake","carried_panes":[{"pane_id":…,"process_id":51234,"row_count":20,"column_count":78,"terminal_fd":9,"terminal_name":"/dev/ttys009","exit_status":null},…]},"raw_body":{…}}`.
//! The next image reads the header, checks that descriptor 9 is still the master of `/dev/ttys009`,
//! takes it and process 51234 back as that pane, then reads the body and puts the pane's screen
//! back under it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_session::session::state::Session;
use koshi_storage::error::StorageError;
use koshi_terminal::engine::{
    GraphicsEvent, GraphicsTransportState, SynchronizedOutputTransport,
    MAX_GRAPHICS_EVENT_BATCH_COUNT, MAX_GRAPHICS_EVENT_COUNT,
};
use koshi_terminal::graphics::{GraphicsError, MAX_IMAGE_BYTE_COUNT};
use koshi_terminal::state::TerminalState;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

mod migration;

/// The resume-file format this build writes.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::RESUME_FORMAT`]. Named by its full
/// path here, since this constant carries the same name.
pub const RESUME_FORMAT: u32 = koshi_core::compat::RESUME_FORMAT.maximum_version;

/// The oldest resume-file format this build reads.
pub const RESUME_FORMAT_MIN: u32 = koshi_core::compat::RESUME_FORMAT.minimum_version;

/// The line a pane shows when the session came back but that pane's screen
/// did not: the program in it keeps running on a blank screen.
pub const SCREEN_NOT_RESTORED_NOTICE_BYTES: &[u8] = b"[koshi] This pane's screen could not be restored after the restart. The program in it is still running.\r\n";

/// The line every pane shows when its session came back without its tabs and
/// splits: each pane sits in a tab of its own on a blank screen, and the
/// program in it keeps running.
pub const LAYOUT_NOT_RESTORED_NOTICE_BYTES: &[u8] = b"[koshi] The session's layout could not be restored after the restart. Each pane now has its own tab, and the program in it is still running.\r\n";

/// The line the one fresh shell shows when none of the session's panes came
/// back.
pub const SESSION_NOT_RESTORED_NOTICE_BYTES: &[u8] = b"[koshi] The session could not be restored after the restart. This is a new shell; the previous panes are unavailable.\r\n";

/// One live pane, as the header names it: what the next image needs to take
/// the pane back, or to shut it down when the body is unreadable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The process id of the pane's child.
    pub process_id: u32,
    /// Height in cells of the pane's terminal.
    pub row_count: u16,
    /// Width in cells of the pane's terminal.
    pub column_count: u16,
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
    /// wrote this file. The next image reports this status and does not wait on
    /// the process id.
    ///
    /// `None` says the child was still running and the next image waits on it
    /// itself. It is also what a header written by a build that recorded no
    /// status carries.
    #[serde(default)]
    pub exit_status: Option<ExitStatus>,
}

impl CarriedPane {
    /// The pane's terminal size, as [`row_count`](Self::row_count) and
    /// [`column_count`](Self::column_count) name it.
    #[must_use]
    pub fn get_pty_size(&self) -> PtySize {
        PtySize {
            row_count: self.row_count,
            column_count: self.column_count,
        }
    }
}

/// The half of the resume file whose shape never changes: which session this
/// is, and every pane it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeHeader {
    /// Which format the body is written in.
    pub resume_format: u32,
    /// The session the writing process serves.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
    /// Every live pane, in the order the PTY backend reported them.
    pub carried_panes: Vec<CarriedPane>,
}

/// The half of the resume file that [`RESUME_FORMAT`] numbers. Its fields below
/// are what a session server hands the image replacing it.
#[derive(Debug, Serialize)]
pub struct ResumeBody {
    /// Every session the writing process held, keyed by id. Each one owns its
    /// tabs, layout trees, pane records and attached clients.
    pub session_by_id: HashMap<SessionId, Session>,
    /// Each pane's terminal, keyed by pane id. A pane with no entry comes back
    /// with a blank screen.
    pub carried_pane_state_by_pane_id: HashMap<PaneId, CarriedPaneState>,
    /// A quit that was applied and not yet carried out, and how it must be
    /// carried out.
    ///
    /// A `core:quit` can land after the clients have been told the session is
    /// restarting and are already waiting for its next socket. The swap runs to
    /// the end, and the next image carries the quit out once every carried
    /// client is back or its window has closed.
    ///
    /// The kind travels with it: a caller that asked for a zero-grace teardown
    /// gets one from the next image too.
    pub carried_quit: Option<CarriedQuit>,
}

/// One pane's terminal, as the writing process left it.
#[derive(Debug, Serialize, Deserialize)]
pub struct CarriedPaneState {
    /// The pane's screen state: grids, scrollback, modes and cursor. The parser
    /// that fed it is not carried; `undecoded_bytes` carries that parser's
    /// position.
    pub terminal_state: TerminalState,
    /// The bytes that put the pane's next parser where the last one stood,
    /// exactly as
    /// [`TerminalEngine::get_undecoded_terminal_bytes`](koshi_terminal::engine::TerminalEngine::get_undecoded_terminal_bytes)
    /// reports them. The next image hands them to
    /// [`TerminalEngine::from_carried_state`](koshi_terminal::engine::TerminalEngine::from_carried_state).
    pub undecoded_bytes: Vec<u8>,
    /// Complete image records and recoverable image errors waiting for the
    /// pane's terminal caller. The next image restores them before it reads new
    /// PTY output. At most [`MAX_GRAPHICS_EVENT_COUNT`] events and one
    /// queue-full report after them, holding at most [`MAX_IMAGE_BYTE_COUNT`]
    /// image bytes in all.
    #[serde(deserialize_with = "deserialize_graphics_events")]
    pub graphics_events: Vec<GraphicsEvent>,
    /// The complete graphics-parser state, including parser state nested
    /// inside a split tmux or GNU Screen wrapper. `None` while the graphics
    /// parser sits in ground state. A Screen-wrapped iTerm2 command split after
    /// `File=` is represented by `screen_inner_transport` here.
    pub graphics_transport: Option<GraphicsTransportState>,
    /// The pane's open synchronized-output group, if one is open.
    pub synchronized_output: Option<SynchronizedOutputTransport>,
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
/// text it was written as. A body whose text is valid JSON of a shape this
/// build cannot decode costs the caller no part of the header.
/// [`read_resume_body`] decodes that text once.
#[derive(Debug, Deserialize)]
struct ResumeFile {
    header: ResumeHeader,
    raw_body: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousResumeFile {
    header: PreviousResumeHeader,
    body: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousResumeHeader {
    format: u32,
    session_id: SessionId,
    session_name: String,
    panes: Vec<PreviousCarriedPane>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousCarriedPane {
    pane_id: PaneId,
    pid: u32,
    rows: u16,
    cols: u16,
    terminal_fd: Option<i32>,
    #[serde(default)]
    terminal_name: Option<String>,
    #[serde(default)]
    exit: Option<ExitStatus>,
}

/// The body as it is read: the session-wide fields decoded, and each pane's
/// state left as the raw JSON text it was written as.
#[derive(Deserialize)]
struct EncodedResumeBody {
    session_by_id: HashMap<SessionId, Session>,
    carried_pane_state_by_pane_id: EncodedPaneStates,
    carried_quit: Option<CarriedQuit>,
}

/// Each pane's carried state as raw JSON text, keyed by pane id, and the keys
/// that name no pane or name one pane more than once.
#[derive(Default)]
struct EncodedPaneStates {
    /// The raw state of every pane whose key reads as a pane id and appears
    /// once.
    raw_pane_state_by_pane_id: HashMap<PaneId, Box<RawValue>>,
    /// Every key that does not read as a pane id, or that appears more than
    /// once, as it was written.
    refused_pane_keys: Vec<String>,
}

/// The same two halves as [`ResumeFile`], borrowed for the write so no pane's
/// grid or scrollback is copied on its way to the disk.
#[derive(Debug, Serialize)]
struct ResumeFileReference<'a> {
    header: &'a ResumeHeader,
    raw_body: &'a ResumeBody,
}

/// Write `header` and `resume_body` to `resume_file_path`, replacing whatever is there.
///
/// The bytes land through [`koshi_storage::atomic::write_atomic`]: a reader
/// finds the whole old file or the whole new one, never a half-written middle.
///
/// # Errors
/// Returns [`StorageError::Io`] when the state cannot be encoded, or when the
/// write does not land durably.
pub fn write_resume_file(
    resume_file_path: &Path,
    header: &ResumeHeader,
    resume_body: &ResumeBody,
) -> Result<(), StorageError> {
    let resume_file_bytes = serde_json::to_vec(&ResumeFileReference {
        header,
        raw_body: resume_body,
    })
    .map_err(|serialization_error| StorageError::Io {
        detail: format!(
            "encode resume state for {}: {serialization_error}",
            resume_file_path.display()
        ),
    })?;
    koshi_storage::atomic::write_atomic(resume_file_path, &resume_file_bytes)
}

/// Read the resume file at `resume_file_path`: its header, and its body as raw JSON for
/// [`read_resume_body`].
///
/// This reads the current header or converts a header written with formats 1
/// through 3. It leaves the body as raw JSON, so a caller can still take the
/// panes back when their saved screens cannot be decoded.
///
/// # Errors
/// Returns [`StorageError::Io`] when the file cannot be read, and
/// [`StorageError::Corrupt`] when its bytes are not a resume file.
pub fn read_resume_header(
    resume_file_path: &Path,
) -> Result<(ResumeHeader, Box<RawValue>), StorageError> {
    let resume_file_bytes =
        std::fs::read(resume_file_path).map_err(|read_error| StorageError::Io {
            detail: format!(
                "read resume state at {}: {read_error}",
                resume_file_path.display()
            ),
        })?;
    if let Ok(resume_file) = serde_json::from_slice::<ResumeFile>(&resume_file_bytes) {
        return Ok((resume_file.header, resume_file.raw_body));
    }
    let previous_resume_file: PreviousResumeFile = serde_json::from_slice(&resume_file_bytes)
        .map_err(|parse_error| StorageError::Corrupt {
            detail: format!(
                "resume state at {} is unreadable: {parse_error}",
                resume_file_path.display()
            ),
        })?;
    if !(RESUME_FORMAT_MIN..RESUME_FORMAT).contains(&previous_resume_file.header.format) {
        return Err(StorageError::Corrupt {
            detail: format!(
                "resume format {} is outside the {} to {} range this build reads",
                previous_resume_file.header.format, RESUME_FORMAT_MIN, RESUME_FORMAT
            ),
        });
    }
    Ok((
        ResumeHeader {
            resume_format: previous_resume_file.header.format,
            session_id: previous_resume_file.header.session_id,
            session_name: previous_resume_file.header.session_name,
            carried_panes: previous_resume_file
                .header
                .panes
                .into_iter()
                .map(|carried_pane| CarriedPane {
                    pane_id: carried_pane.pane_id,
                    process_id: carried_pane.pid,
                    row_count: carried_pane.rows,
                    column_count: carried_pane.cols,
                    terminal_fd: carried_pane.terminal_fd,
                    terminal_name: carried_pane.terminal_name,
                    exit_status: carried_pane.exit,
                })
                .collect(),
        },
        previous_resume_file.body,
    ))
}

/// Decode the raw `resume_body` [`read_resume_header`] handed back, given the `resume_format` the
/// same header named.
///
/// The sessions and carried quit are read as one. Each pane's
/// [`CarriedPaneState`] is read on its own. A pane whose key is no pane id or
/// appears twice is logged and left out. An unreadable format 4 pane is also
/// left out. An unreadable pane converted from formats 1 through 3 makes the
/// body unreadable.
///
/// Example: a format 4 body carries panes `A` and `B`, where `B`'s screen
/// holds a Kitty placement no upload holds. The returned body carries `A`
/// alone, and a warning names `B`.
///
/// # Errors
/// Returns [`StorageError::Corrupt`] when `format` is outside
/// `RESUME_FORMAT_MIN..=RESUME_FORMAT`, and when the sessions, the carried quit
/// or the map of pane states is not that format's shape, or when a pane
/// converted from formats 1 through 3 cannot be decoded.
pub fn read_resume_body(
    resume_format: u32,
    resume_body: &RawValue,
) -> Result<ResumeBody, StorageError> {
    if !(RESUME_FORMAT_MIN..=RESUME_FORMAT).contains(&resume_format) {
        return Err(StorageError::Corrupt {
            detail: format!(
                "resume body format {resume_format} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
            ),
        });
    }
    if resume_format < RESUME_FORMAT {
        return migration::migrate_resume_body(resume_format, resume_body.get());
    }
    let encoded_resume_body: EncodedResumeBody =
        serde_json::from_str(resume_body.get()).map_err(|parse_error| StorageError::Corrupt {
            detail: format!("resume body is unreadable: {parse_error}"),
        })?;
    for refused_pane_key in &encoded_resume_body
        .carried_pane_state_by_pane_id
        .refused_pane_keys
    {
        tracing::warn!(
            pane_key = %refused_pane_key,
            "a carried pane state is keyed by no pane id or by one named twice; that pane comes back with a blank screen"
        );
    }
    let mut carried_pane_state_by_pane_id = HashMap::new();
    for (pane_id, raw_pane_state) in encoded_resume_body
        .carried_pane_state_by_pane_id
        .raw_pane_state_by_pane_id
    {
        match serde_json::from_str::<CarriedPaneState>(raw_pane_state.get()) {
            Ok(carried_pane_state) => {
                carried_pane_state_by_pane_id.insert(pane_id, carried_pane_state);
            }
            Err(parse_error) => tracing::warn!(
                %pane_id,
                %parse_error,
                "a carried pane state could not be read; that pane comes back with a blank screen"
            ),
        }
    }
    Ok(ResumeBody {
        session_by_id: encoded_resume_body.session_by_id,
        carried_pane_state_by_pane_id,
        carried_quit: encoded_resume_body.carried_quit,
    })
}

impl<'de> Deserialize<'de> for EncodedPaneStates {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(EncodedPaneStatesVisitor)
    }
}

struct EncodedPaneStatesVisitor;

impl<'de> Visitor<'de> for EncodedPaneStatesVisitor {
    type Value = EncodedPaneStates;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a map of pane ids to carried pane states")
    }

    /// Keep every entry whose key reads as a pane id and appears once. A key
    /// that reads as no pane id, and every entry of a key that appears more
    /// than once, goes to `refused_pane_keys` instead.
    fn visit_map<A>(self, mut map_access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut encoded_pane_states = EncodedPaneStates::default();
        let mut repeated_pane_ids: HashSet<PaneId> = HashSet::new();
        while let Some(pane_key) = map_access.next_key::<String>()? {
            let raw_pane_state = map_access.next_value::<Box<RawValue>>()?;
            let Ok(pane_id) =
                serde_json::from_value::<PaneId>(serde_json::Value::String(pane_key.clone()))
            else {
                encoded_pane_states.refused_pane_keys.push(pane_key);
                continue;
            };
            if repeated_pane_ids.contains(&pane_id)
                || encoded_pane_states
                    .raw_pane_state_by_pane_id
                    .remove(&pane_id)
                    .is_some()
            {
                repeated_pane_ids.insert(pane_id);
                encoded_pane_states.refused_pane_keys.push(pane_key);
                continue;
            }
            encoded_pane_states
                .raw_pane_state_by_pane_id
                .insert(pane_id, raw_pane_state);
        }
        Ok(encoded_pane_states)
    }
}

fn deserialize_graphics_events<'de, D>(deserializer: D) -> Result<Vec<GraphicsEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_seq(GraphicsEventListVisitor)
}

struct GraphicsEventListVisitor;

impl<'de> Visitor<'de> for GraphicsEventListVisitor {
    type Value = Vec<GraphicsEvent>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded graphics event list")
    }

    fn visit_seq<A>(self, mut graphics_event_sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let graphics_event_size_hint = graphics_event_sequence.size_hint();
        if graphics_event_size_hint.is_some_and(|graphics_event_count| {
            graphics_event_count > MAX_GRAPHICS_EVENT_BATCH_COUNT
        }) {
            return Err(de::Error::custom(format!(
                "graphics event count exceeds {MAX_GRAPHICS_EVENT_COUNT}"
            )));
        }
        let mut graphics_events = Vec::with_capacity(
            graphics_event_size_hint
                .unwrap_or(0)
                .min(MAX_GRAPHICS_EVENT_BATCH_COUNT),
        );
        let mut decoded_image_byte_count = 0usize;
        while let Some(graphics_event) = graphics_event_sequence.next_element::<GraphicsEvent>()? {
            if graphics_events.len() == MAX_GRAPHICS_EVENT_BATCH_COUNT {
                return Err(de::Error::custom(format!(
                    "graphics event count exceeds {MAX_GRAPHICS_EVENT_COUNT}"
                )));
            }
            if let Err(GraphicsError::QueueFull {
                dropped_event_count,
            }) = &graphics_event
            {
                if *dropped_event_count == 0 || graphics_events.len() != MAX_GRAPHICS_EVENT_COUNT {
                    return Err(de::Error::custom(
                        "graphics queue-full report must follow the event limit",
                    ));
                }
            } else if graphics_events.len() >= MAX_GRAPHICS_EVENT_COUNT {
                return Err(de::Error::custom(format!(
                    "graphics event count exceeds {MAX_GRAPHICS_EVENT_COUNT}"
                )));
            }
            let graphics_event_image_bytes = match &graphics_event {
                Ok(image_record) => image_record.image.rgba_bytes.len(),
                Err(_) => 0,
            };
            decoded_image_byte_count = decoded_image_byte_count
                .checked_add(graphics_event_image_bytes)
                .ok_or_else(|| de::Error::custom("graphics image-byte count overflows"))?;
            if decoded_image_byte_count > MAX_IMAGE_BYTE_COUNT {
                return Err(de::Error::custom(format!(
                    "graphics image bytes exceed {MAX_IMAGE_BYTE_COUNT}"
                )));
            }
            graphics_events.push(graphics_event);
        }
        Ok(graphics_events)
    }
}

#[cfg(test)]
mod tests;
