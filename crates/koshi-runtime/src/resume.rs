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
//! [`ResumeHeader::resume_format`] numbers it: [`RESUME_FORMAT`] is what this build
//! writes, [`RESUME_FORMAT_MIN`] the oldest it reads, and [`read_resume_body`] refuses
//! anything outside that range. Formats 1 and 2 differ in one key: format 1
//! carries a `tier` key on every attached client, format 2 carries none.
//! Format 3 carries prompt metadata with every terminal row.
//!
//! Example: a server holding two panes writes
//! `{"header":{"format":3,…,"panes":[{"pane_id":…,"pid":51234,"rows":20,"cols":78,"terminal_fd":9,"terminal_name":"/dev/ttys009","exit":null},…]},"body":{…}}`.
//! The next image reads the header, checks that descriptor 9 is still the
//! master of `/dev/ttys009`, takes it and process 51234 back as that pane, then
//! reads the body and puts the pane's screen back under it.

use std::collections::HashMap;
use std::path::Path;

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_session::session::state::Session;
use koshi_storage::error::StorageError;
use koshi_terminal::engine::{
    GraphicsEvent, GraphicsTransportState, SynchronizedOutputTransport,
    MAX_GRAPHICS_EVENT_BATCH_COUNT, MAX_GRAPHICS_EVENT_COUNT,
};
use koshi_terminal::graphics::{
    GraphicsError, MAX_GRAPHICS_CARRY_BYTE_COUNT, MAX_IMAGE_BYTE_COUNT,
};
use koshi_terminal::state::TerminalState;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// The resume-file format this build writes.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::RESUME_FORMAT`]. Named by its full
/// path here, since this constant carries the same name.
pub const RESUME_FORMAT: u32 = koshi_core::compat::RESUME_FORMAT.maximum_version;

/// The oldest resume-file format this build reads.
pub const RESUME_FORMAT_MIN: u32 = koshi_core::compat::RESUME_FORMAT.minimum_version;

/// One live pane, as the header names it: what the next image needs to take
/// the pane back, or to shut it down when the body is unreadable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The process id of the pane's child.
    #[serde(rename = "pid")]
    pub process_id: u32,
    /// Height in cells of the pane's terminal.
    #[serde(rename = "rows")]
    pub row_count: u16,
    /// Width in cells of the pane's terminal.
    #[serde(rename = "cols")]
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
    #[serde(default, rename = "exit")]
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
    #[serde(rename = "format")]
    pub resume_format: u32,
    /// The session the writing process serves.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
    /// Every live pane, in the order the PTY backend reported them.
    #[serde(rename = "panes")]
    pub carried_panes: Vec<CarriedPane>,
}

/// The half of the resume file that [`RESUME_FORMAT`] numbers. Its fields below
/// are what a session server hands the image replacing it.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeBody {
    /// Every session the writing process held, keyed by id. Each one owns its
    /// tabs, layout trees, pane records and attached clients.
    #[serde(rename = "sessions")]
    pub session_by_id: HashMap<SessionId, Session>,
    /// Each pane's screen state, keyed by pane id: grids, scrollback, modes and
    /// cursor. The parser that fed it is not carried; `undecoded` carries that
    /// parser's position.
    #[serde(rename = "engines")]
    pub terminal_state_by_pane_id: HashMap<PaneId, TerminalState>,
    /// The bytes that put each pane's next parser where the last one stood,
    /// keyed by pane id, exactly as
    /// [`TerminalEngine::undecoded_terminal_bytes`](koshi_terminal::engine::TerminalEngine::undecoded_terminal_bytes)
    /// reports them; a pane it reports nothing for has no entry. The next image
    /// hands an entry to
    /// [`TerminalEngine::from_terminal_state`](koshi_terminal::engine::TerminalEngine::from_terminal_state).
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(rename = "undecoded", default)]
    pub undecoded_bytes_by_pane_id: HashMap<PaneId, Vec<u8>>,
    /// The raw bytes that put each pane's graphics parser where the last one
    /// stood, keyed by pane id. The next image uses this compatibility field
    /// with [`TerminalEngine::from_terminal_state_with_graphics`](koshi_terminal::engine::TerminalEngine::from_terminal_state_with_graphics)
    /// when no nested wrapper state is present.
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(
        rename = "graphics_undecoded",
        default,
        deserialize_with = "deserialize_graphics_undecoded"
    )]
    pub graphics_undecoded_bytes_by_pane_id: HashMap<PaneId, Vec<u8>>,
    /// Whether each pane's graphics parser expects the next DCS to carry the
    /// next GNU Screen passthrough fragment.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(rename = "graphics_screen_continuation", default)]
    pub graphics_screen_continuation_by_pane_id: HashMap<PaneId, bool>,
    /// Whether each pane's carried graphics bytes are inside a GNU Screen
    /// passthrough DCS string.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(rename = "graphics_screen_wrapper_active", default)]
    pub graphics_screen_wrapper_active_by_pane_id: HashMap<PaneId, bool>,
    /// Whether each pane's graphics parser expects the next DCS to carry the
    /// next tmux passthrough fragment.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(rename = "graphics_tmux_continuation", default)]
    pub graphics_tmux_continuation_by_pane_id: HashMap<PaneId, bool>,
    /// Whether each pane's carried graphics bytes are inside an open tmux
    /// passthrough DCS string.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(rename = "graphics_tmux_wrapper_active", default)]
    pub graphics_tmux_wrapper_active_by_pane_id: HashMap<PaneId, bool>,
    /// Complete image records and recoverable image errors waiting for each
    /// pane's terminal caller, keyed by pane id. The next image restores them
    /// before it reads new PTY output.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(
        rename = "graphics_events",
        default,
        deserialize_with = "deserialize_graphics_events"
    )]
    pub graphics_events_by_pane_id: HashMap<PaneId, Vec<GraphicsEvent>>,
    /// The complete graphics-parser state for each pane, including parser
    /// state nested inside a split tmux or GNU Screen wrapper. The next image
    /// restores it before reading new PTY output. A Screen-wrapped iTerm2
    /// command split after `File=` is represented by `screen_inner` here.
    ///
    /// A body whose JSON carries no map for this field reads back as an empty
    /// one.
    #[serde(default)]
    #[serde(rename = "graphics_transport")]
    pub graphics_transport_by_pane_id: HashMap<PaneId, GraphicsTransportState>,
    /// Open synchronized-output groups keyed by pane id.
    #[serde(default)]
    #[serde(rename = "synchronized_output")]
    pub synchronized_output_by_pane_id: HashMap<PaneId, SynchronizedOutputTransport>,
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
    ///
    /// A body whose JSON carries no value for this field reads back as `None`.
    #[serde(default)]
    #[serde(rename = "quit")]
    pub carried_quit: Option<CarriedQuit>,
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
    #[serde(rename = "body")]
    raw_body: Box<RawValue>,
}

/// The same two halves as [`ResumeFile`], borrowed for the write so no pane's
/// grid or scrollback is copied on its way to the disk.
#[derive(Debug, Serialize)]
struct ResumeFileRef<'a> {
    header: &'a ResumeHeader,
    body: &'a ResumeBody,
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
    let resume_file_bytes = serde_json::to_vec(&ResumeFileRef {
        header,
        body: resume_body,
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
/// The header's shape never changes, so this call answers for a file any build
/// wrote. It does not look at [`ResumeHeader::resume_format`], so a caller holding a
/// body it cannot read still gets every pane's descriptor and process id.
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
    let resume_file: ResumeFile =
        serde_json::from_slice(&resume_file_bytes).map_err(|parse_error| {
            StorageError::Corrupt {
                detail: format!(
                    "resume state at {} is unreadable: {parse_error}",
                    resume_file_path.display()
                ),
            }
        })?;
    Ok((resume_file.header, resume_file.raw_body))
}

/// Decode the raw `resume_body` [`read_resume_header`] handed back, given the `resume_format` the
/// same header named.
///
/// # Errors
/// Returns [`StorageError::Corrupt`] when `format` is outside
/// `RESUME_FORMAT_MIN..=RESUME_FORMAT`, and when the body is not that format's
/// shape.
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
    serde_json::from_str(resume_body.get()).map_err(|parse_error| StorageError::Corrupt {
        detail: format!("resume body is unreadable: {parse_error}"),
    })
}

fn deserialize_graphics_events<'de, D>(
    deserializer: D,
) -> Result<HashMap<PaneId, Vec<GraphicsEvent>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_map(GraphicsEventsVisitor)
}

fn deserialize_graphics_undecoded<'de, D>(
    deserializer: D,
) -> Result<HashMap<PaneId, Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_map(GraphicsUndecodedVisitor)
}

struct GraphicsUndecodedVisitor;

impl<'de> Visitor<'de> for GraphicsUndecodedVisitor {
    type Value = HashMap<PaneId, Vec<u8>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a map of pane ids to bounded graphics carry bytes")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut graphics_carry_bytes_by_pane_id = HashMap::new();
        while let Some(pane_id) = map.next_key::<PaneId>()? {
            if graphics_carry_bytes_by_pane_id.contains_key(&pane_id) {
                return Err(de::Error::custom("duplicate graphics carry pane id"));
            }
            let graphics_carry_bytes = map.next_value_seed(GraphicsCarrySeed)?;
            graphics_carry_bytes_by_pane_id.insert(pane_id, graphics_carry_bytes);
        }
        Ok(graphics_carry_bytes_by_pane_id)
    }
}

struct GraphicsCarrySeed;

impl<'de> DeserializeSeed<'de> for GraphicsCarrySeed {
    type Value = Vec<u8>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(GraphicsCarryVisitor)
    }
}

struct GraphicsCarryVisitor;

impl<'de> Visitor<'de> for GraphicsCarryVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded graphics carry bytes")
    }

    fn visit_seq<A>(self, mut graphics_carry_byte_sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let graphics_carry_size_hint = graphics_carry_byte_sequence.size_hint();
        if graphics_carry_size_hint
            .is_some_and(|byte_count| byte_count > MAX_GRAPHICS_CARRY_BYTE_COUNT)
        {
            return Err(de::Error::custom(format!(
                "graphics carry exceeds {MAX_GRAPHICS_CARRY_BYTE_COUNT} bytes"
            )));
        }
        let mut graphics_carry_bytes = Vec::with_capacity(
            graphics_carry_size_hint
                .unwrap_or(0)
                .min(MAX_GRAPHICS_CARRY_BYTE_COUNT),
        );
        while let Some(graphics_carry_byte) = graphics_carry_byte_sequence.next_element::<u8>()? {
            if graphics_carry_bytes.len() == MAX_GRAPHICS_CARRY_BYTE_COUNT {
                return Err(de::Error::custom(format!(
                    "graphics carry exceeds {MAX_GRAPHICS_CARRY_BYTE_COUNT} bytes"
                )));
            }
            graphics_carry_bytes.push(graphics_carry_byte);
        }
        Ok(graphics_carry_bytes)
    }

    fn visit_bytes<E>(self, graphics_carry_bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if graphics_carry_bytes.len() > MAX_GRAPHICS_CARRY_BYTE_COUNT {
            return Err(E::custom(format!(
                "graphics carry exceeds {MAX_GRAPHICS_CARRY_BYTE_COUNT} bytes"
            )));
        }
        Ok(graphics_carry_bytes.to_vec())
    }

    fn visit_byte_buf<E>(self, graphics_carry_bytes: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_bytes(&graphics_carry_bytes)
    }
}

struct GraphicsEventsVisitor;

impl<'de> Visitor<'de> for GraphicsEventsVisitor {
    type Value = HashMap<PaneId, Vec<GraphicsEvent>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a map of pane ids to bounded graphics event lists")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut graphics_events_by_pane_id = HashMap::new();
        while let Some(pane_id) = map.next_key::<PaneId>()? {
            if graphics_events_by_pane_id.contains_key(&pane_id) {
                return Err(de::Error::custom("duplicate graphics event pane id"));
            }
            let graphics_events = map.next_value_seed(GraphicsEventListSeed)?;
            graphics_events_by_pane_id.insert(pane_id, graphics_events);
        }
        Ok(graphics_events_by_pane_id)
    }
}

struct GraphicsEventListSeed;

impl<'de> DeserializeSeed<'de> for GraphicsEventListSeed {
    type Value = Vec<GraphicsEvent>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(GraphicsEventListVisitor)
    }
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
