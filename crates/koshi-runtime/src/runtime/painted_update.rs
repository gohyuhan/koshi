//! Both ends of the painted updates on one attached client's connection
//! ([`koshi_ipc::painted_update`]).
//!
//! [`PaintedUpdateSender`] lives on the thread that writes the connection. It
//! holds the rows it last sent for each pane, and turns each new
//! [`RenderSnapshot`] into the binary frames of one update: the rows the
//! client holds again by row index or copied from another row index, and only
//! the other rows in the row encoding. A row's identity is its
//! `Arc<RowSnapshot>`, and a row whose encoded bytes equal the held row's at
//! the same index is held again too.
//!
//! [`PaintedUpdateReceiver`] lives on the thread that reads the connection. It
//! holds the same rows, applies each update's parts in order, and gives back
//! one [`ReceivedPaintedFrame`] at each update's last part.
//! [`PaintedUpdateReceiver::accept_session_frame`] takes every frame the
//! connection reads, JSON and binary alike, and gives back each JSON message,
//! each image chunk and each applied update in the order they arrived.
//!
//! Example — a keystroke in a 200×50 pane: the sender sends `keep 20`, one new
//! row of about 215 bytes, `keep 29`, and the cursor. The receiver rebuilds the
//! 50 rows from the 49 it holds and the one it decoded.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use koshi_core::ids::PaneId;
use koshi_ipc::error::IpcError;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::image_chunk::ImageChunkFrame;
use koshi_ipc::painted_update::{
    decode_session_binary_frame, encode_painted_update, PaintedUpdateEncodeError,
    PaintedUpdatePart, PaneCursor, PaneCursorShape, PaneHeader, PaneSection, PaneUpdate,
    PaneWindow, RowInstruction, SessionBinaryFrame,
};
use koshi_ipc::transport::ReceivedFrame;
use koshi_renderer::snapshot::{CursorSnapshot, GridView, PaneSnapshot, RenderSnapshot};
use koshi_terminal::grid::row_snapshot::RowSnapshot;
use koshi_terminal::row_encoding::RowDecodeError;
use koshi_terminal::state::CursorShape;

/// What one connection's writer sent last: the JSON, the rows of each pane,
/// and the number of the next update.
#[derive(Debug)]
pub struct PaintedUpdateSender {
    /// The JSON part of the last update sent, or empty before the first.
    sent_structure_json_bytes: Vec<u8>,
    /// What the last update sent for each pane.
    sent_pane_by_id: HashMap<PaneId, SentPane>,
    /// The number the next update carries.
    next_update_sequence: u64,
}

/// What the writer sent last for one pane.
#[derive(Debug)]
struct SentPane {
    /// The header as the client holds it: its window's `is_reset` is `false`.
    pane_header: PaneHeader,
    /// Whether the pane showed its alternate screen.
    is_on_alternate_screen: bool,
    /// The rows of the pane's window, top row first. Empty for a pane that
    /// shows no window.
    row_snapshots: Vec<Arc<RowSnapshot>>,
}

impl Default for PaintedUpdateSender {
    fn default() -> Self {
        Self::new()
    }
}

impl PaintedUpdateSender {
    /// A sender that has sent nothing: its first update carries the JSON and
    /// every row, and is number 1.
    #[must_use]
    pub fn new() -> Self {
        PaintedUpdateSender {
            sent_structure_json_bytes: Vec::new(),
            sent_pane_by_id: HashMap::new(),
            next_update_sequence: 1,
        }
    }

    /// The number of the last update this sender built, or `0` before the
    /// first.
    #[must_use]
    pub fn get_last_update_sequence(&self) -> u64 {
        self.next_update_sequence - 1
    }

    /// The binary frame payloads of the update that turns what this sender
    /// sent last into `render_snapshot`, whose JSON part is `painted_frame`.
    /// `None` when nothing changed and `is_update_required` is `false`: the
    /// client already shows `render_snapshot`. Building the update records it
    /// as sent.
    ///
    /// A pane the sender did not send last, a pane whose window changes its
    /// column count or its row count, and a pane that enters or leaves the
    /// alternate screen send every row.
    ///
    /// # Errors
    /// The [`PaintedUpdateEncodeError`] of an update whose JSON or one of
    /// whose rows does not fit one frame. Nothing is recorded as sent then.
    ///
    /// # Panics
    /// When `painted_frame` does not encode as JSON, or a pane's window holds
    /// more than 65 535 rows.
    pub fn build_update_frames(
        &mut self,
        render_snapshot: &RenderSnapshot,
        painted_frame: &PaintedFrame,
        is_update_required: bool,
    ) -> Result<Option<Vec<Vec<u8>>>, PaintedUpdateEncodeError> {
        let structure_json_bytes =
            serde_json::to_vec(painted_frame).expect("a painted frame encodes as JSON");
        let is_structure_unchanged = structure_json_bytes == self.sent_structure_json_bytes;
        let mut is_every_pane_unchanged = true;
        let mut pane_updates = Vec::with_capacity(render_snapshot.pane_snapshots.len());
        for pane_snapshot in &render_snapshot.pane_snapshots {
            let sent_pane = self.sent_pane_by_id.get(&pane_snapshot.pane_id);
            let pane_update = build_pane_update(pane_snapshot, sent_pane);
            is_every_pane_unchanged &= is_pane_update_unchanged(&pane_update, sent_pane);
            pane_updates.push(pane_update);
        }
        if is_structure_unchanged && is_every_pane_unchanged && !is_update_required {
            return Ok(None);
        }
        let part_payloads = encode_painted_update(
            self.next_update_sequence,
            (!is_structure_unchanged).then_some(structure_json_bytes.as_slice()),
            &pane_updates,
        )?;
        self.sent_pane_by_id = render_snapshot
            .pane_snapshots
            .iter()
            .zip(&pane_updates)
            .map(|(pane_snapshot, pane_update)| {
                (
                    pane_snapshot.pane_id,
                    SentPane {
                        pane_header: PaneHeader {
                            pane_window: pane_update.pane_header.pane_window.map(|pane_window| {
                                PaneWindow {
                                    is_reset: false,
                                    ..pane_window
                                }
                            }),
                            ..pane_update.pane_header
                        },
                        is_on_alternate_screen: pane_snapshot.is_on_alternate_screen,
                        row_snapshots: pane_snapshot
                            .terminal_grid_view
                            .as_ref()
                            .map(|grid_view| grid_view.row_snapshots.clone())
                            .unwrap_or_default(),
                    },
                )
            })
            .collect();
        self.sent_structure_json_bytes = structure_json_bytes;
        self.next_update_sequence += 1;
        Ok(Some(part_payloads))
    }
}

/// The update of one pane against `sent_pane`, what the writer sent last for
/// it.
fn build_pane_update<'row>(
    pane_snapshot: &'row PaneSnapshot,
    sent_pane: Option<&SentPane>,
) -> PaneUpdate<'row> {
    let cursor_snapshot = &pane_snapshot.cursor_snapshot;
    let cursor = PaneCursor {
        row_index: cursor_snapshot.row_index,
        column_index: cursor_snapshot.column_index,
        is_visible: cursor_snapshot.is_visible,
        is_blinking: cursor_snapshot.is_blinking,
        shape: cursor_snapshot
            .shape
            .map(|cursor_shape| match cursor_shape {
                CursorShape::Block => PaneCursorShape::Block,
                CursorShape::Underline => PaneCursorShape::Underline,
                CursorShape::Bar => PaneCursorShape::Bar,
            }),
    };
    let (pane_window, row_instructions) = match &pane_snapshot.terminal_grid_view {
        None => (None, Vec::new()),
        Some(grid_view) => {
            let row_count = u16::try_from(grid_view.row_snapshots.len())
                .expect("a pane window holds at most 65 535 rows");
            let held_row_snapshots = sent_pane
                .filter(|sent_pane| {
                    sent_pane.is_on_alternate_screen == pane_snapshot.is_on_alternate_screen
                        && sent_pane
                            .pane_header
                            .pane_window
                            .is_some_and(|sent_pane_window| {
                                sent_pane_window.column_count == grid_view.column_count
                                    && sent_pane_window.row_count == row_count
                            })
                })
                .map(|sent_pane| sent_pane.row_snapshots.as_slice());
            let pane_window = PaneWindow {
                column_count: grid_view.column_count,
                row_count,
                view_row_offset: grid_view.view_row_offset as u64,
                is_reset: held_row_snapshots.is_none(),
            };
            let row_instructions = compute_row_instructions(
                held_row_snapshots.unwrap_or_default(),
                &grid_view.row_snapshots,
            );
            (Some(pane_window), row_instructions)
        }
    };
    PaneUpdate {
        pane_id: pane_snapshot.pane_id,
        pane_header: PaneHeader {
            cursor,
            view_top_row_index: pane_snapshot.view_top_row_index,
            retained_line_count: pane_snapshot.scrollback_metadata.retained_line_count as u64,
            pane_window,
        },
        row_instructions,
    }
}

/// Whether `pane_update` changes nothing the client holds for its pane:
/// `sent_pane` was sent with the same header, and every row is kept.
fn is_pane_update_unchanged(pane_update: &PaneUpdate<'_>, sent_pane: Option<&SentPane>) -> bool {
    sent_pane.is_some_and(|sent_pane| sent_pane.pane_header == pane_update.pane_header)
        && pane_update
            .row_instructions
            .iter()
            .all(|row_instruction| matches!(row_instruction, RowInstruction::Keep { .. }))
}

/// The instructions that build `shown_row_snapshots` from `held_row_snapshots`:
/// a row equal to the held row at its index is kept, a row that is a held row
/// at another index is copied, and every other row is new. Neighboring keeps
/// join into one, and so do copies of neighboring held rows.
///
/// Example — held rows `a b c`, shown rows `b c d` after one line scrolled
/// out: `copy 1 2`, then `new d`.
fn compute_row_instructions<'row>(
    held_row_snapshots: &[Arc<RowSnapshot>],
    shown_row_snapshots: &'row [Arc<RowSnapshot>],
) -> Vec<RowInstruction<&'row [u8]>> {
    let mut row_instructions: Vec<RowInstruction<&'row [u8]>> = Vec::new();
    let mut held_row_index_by_address: Option<HashMap<*const RowSnapshot, u16>> = None;
    for (shown_row_index, shown_row_snapshot) in shown_row_snapshots.iter().enumerate() {
        let is_kept = held_row_snapshots
            .get(shown_row_index)
            .is_some_and(|held_row_snapshot| {
                Arc::ptr_eq(held_row_snapshot, shown_row_snapshot)
                    || held_row_snapshot.get_encoded_row_bytes()
                        == shown_row_snapshot.get_encoded_row_bytes()
            });
        if is_kept {
            match row_instructions.last_mut() {
                Some(RowInstruction::Keep { row_count }) => *row_count += 1,
                _ => row_instructions.push(RowInstruction::Keep { row_count: 1 }),
            }
            continue;
        }
        let held_row_index_by_address = held_row_index_by_address.get_or_insert_with(|| {
            (0_u16..)
                .zip(held_row_snapshots)
                .map(|(held_row_index, held_row_snapshot)| {
                    (Arc::as_ptr(held_row_snapshot), held_row_index)
                })
                .collect()
        });
        if let Some(&source_row_index) =
            held_row_index_by_address.get(&Arc::as_ptr(shown_row_snapshot))
        {
            match row_instructions.last_mut() {
                Some(RowInstruction::Copy {
                    source_row_index: run_source_row_index,
                    row_count,
                }) if *run_source_row_index + *row_count == source_row_index => *row_count += 1,
                _ => row_instructions.push(RowInstruction::Copy {
                    source_row_index,
                    row_count: 1,
                }),
            }
            continue;
        }
        row_instructions.push(RowInstruction::New {
            encoded_row_bytes: shown_row_snapshot.get_encoded_row_bytes(),
        });
    }
    row_instructions
}

/// One frame of an attached client's session stream, as
/// [`PaintedUpdateReceiver::accept_session_frame`] gives it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFrame<Message> {
    /// A JSON frame, parsed as `Message`.
    Event(Message),
    /// The last part of a painted update arrived, and the update is applied.
    Painted(Box<ReceivedPaintedFrame>),
    /// One chunk of an image record, holding the frame it arrived in.
    ImageChunk(ImageChunkFrame),
}

/// One painted update, applied: the JSON part and every pane's cursor, scroll
/// position and rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPaintedFrame {
    /// The update's number on its connection.
    pub update_sequence: u64,
    /// The JSON part: the update's own, or the one the client held.
    pub painted_frame: Arc<PaintedFrame>,
    /// One view per pane, in the order of
    /// [`PaintedFrame::pane_snapshots`].
    pub pane_views: Vec<ReceivedPaneView>,
}

/// One pane's cursor, scroll position and rows, as an update leaves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPaneView {
    /// The pane.
    pub pane_id: PaneId,
    /// The cursor's position and look within the content area.
    pub cursor_snapshot: CursorSnapshot,
    /// The absolute line number of the top row the pane shows.
    pub view_top_row_index: u64,
    /// How many history lines the pane retains.
    pub retained_line_count: usize,
    /// The pane's rows. `None` for a pane that shows no window.
    pub terminal_grid_view: Option<GridView>,
}

/// Why [`PaintedUpdateReceiver::accept_painted_update_part`] refused a part.
/// The connection's rows are unknown after any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaintedUpdateError {
    /// A part opens an update while another update is open.
    UpdateAlreadyOpen,
    /// A part that does not open an update arrived with no update open.
    MissingUpdateOpening,
    /// An update's number is not one more than the last update's.
    UpdateOutOfSequence {
        /// The number the receiver expects.
        expected_update_sequence: u64,
        /// The number the update carries.
        received_update_sequence: u64,
    },
    /// The first update on a connection carries no JSON part.
    MissingStructure,
    /// A section that goes on with a pane names another pane than the one
    /// open, or no pane is open.
    SectionContinuesNoPane {
        /// The pane the section names.
        pane_id: PaneId,
    },
    /// A pane's rows do not add up to its window's row count.
    PaneRowCountMismatch {
        /// The pane.
        pane_id: PaneId,
        /// The window's row count.
        expected_row_count: u16,
        /// The rows the instructions give.
        received_row_count: usize,
    },
    /// A pane that shows no window carries row instructions.
    RowsWithoutWindow {
        /// The pane.
        pane_id: PaneId,
    },
    /// A keep or a copy names rows the receiver does not hold for the pane.
    HeldRowOutOfRange {
        /// The pane.
        pane_id: PaneId,
    },
    /// A new row does not decode, or is not as wide as its window.
    UndecodableRow {
        /// The pane.
        pane_id: PaneId,
        /// Why the row does not decode.
        row_decode_error: RowDecodeError,
    },
    /// The update's panes are not the panes of its JSON part, in that order:
    /// a pane opens out of that order, or the update ends with a pane of the
    /// JSON part not opened.
    PaneListMismatch,
    /// A count does not fit this machine's `usize`.
    CountTooLarge,
}

impl fmt::Display for PaintedUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpdateAlreadyOpen => {
                formatter.write_str("a painted update opens while another is open")
            }
            Self::MissingUpdateOpening => {
                formatter.write_str("a painted update part arrives with no update open")
            }
            Self::UpdateOutOfSequence {
                expected_update_sequence,
                received_update_sequence,
            } => write!(
                formatter,
                "painted update {received_update_sequence} arrives where {expected_update_sequence} is due"
            ),
            Self::MissingStructure => {
                formatter.write_str("the first painted update carries no painted frame")
            }
            Self::SectionContinuesNoPane { pane_id } => write!(
                formatter,
                "a painted update section goes on with {pane_id}, which is not open"
            ),
            Self::PaneRowCountMismatch {
                pane_id,
                expected_row_count,
                received_row_count,
            } => write!(
                formatter,
                "a painted update gives {pane_id} {received_row_count} rows where its window holds {expected_row_count}"
            ),
            Self::RowsWithoutWindow { pane_id } => write!(
                formatter,
                "a painted update gives rows to {pane_id}, which shows no window"
            ),
            Self::HeldRowOutOfRange { pane_id } => write!(
                formatter,
                "a painted update keeps or copies rows of {pane_id} that are not held"
            ),
            Self::UndecodableRow {
                pane_id,
                row_decode_error,
            } => write!(
                formatter,
                "a painted update row of {pane_id} does not decode: {row_decode_error}"
            ),
            Self::PaneListMismatch => {
                formatter.write_str("a painted update's panes are not its painted frame's panes")
            }
            Self::CountTooLarge => {
                formatter.write_str("a painted update count does not fit this machine")
            }
        }
    }
}

impl std::error::Error for PaintedUpdateError {}

/// What one connection's reader holds: the JSON part and each pane's rows of
/// the last update it applied, and the update it is applying.
#[derive(Debug)]
pub struct PaintedUpdateReceiver {
    /// The JSON part of the last update applied, or `None` before the first.
    held_painted_frame: Option<Arc<PaintedFrame>>,
    /// The rows of each pane with a window, as the last update applied left
    /// them.
    held_pane_by_id: HashMap<PaneId, HeldPane>,
    /// The number of the last update applied, or `0` before the first.
    last_update_sequence: u64,
    /// The update whose parts are arriving, or `None` between updates.
    open_update: Option<OpenUpdate>,
}

/// The rows a reader holds for one pane's window.
#[derive(Debug)]
struct HeldPane {
    /// The window's column count.
    column_count: u16,
    /// The window's rows, top row first.
    row_snapshots: Vec<Arc<RowSnapshot>>,
}

/// An update whose last part has not arrived.
#[derive(Debug)]
struct OpenUpdate {
    /// The update's number.
    update_sequence: u64,
    /// The update's JSON part: its own, or the one the receiver held.
    painted_frame: Arc<PaintedFrame>,
    /// The panes opened so far, in the order of the JSON part's panes.
    staged_panes: Vec<StagedPane>,
}

/// One pane of an open update: its header, and its rows so far.
#[derive(Debug)]
struct StagedPane {
    /// The pane.
    pane_id: PaneId,
    /// The pane's cursor, scroll position and window.
    pane_header: PaneHeader,
    /// The rows built so far, top row first.
    row_snapshots: Vec<Arc<RowSnapshot>>,
}

impl Default for PaintedUpdateReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl PaintedUpdateReceiver {
    /// A receiver that holds nothing: the first update it accepts is number 1
    /// and carries the JSON part.
    #[must_use]
    pub fn new() -> Self {
        PaintedUpdateReceiver {
            held_painted_frame: None,
            held_pane_by_id: HashMap::new(),
            last_update_sequence: 0,
            open_update: None,
        }
    }

    /// Take one frame a client's connection read. A JSON frame is given back
    /// as [`SessionFrame::Event`], and an image chunk frame as
    /// [`SessionFrame::ImageChunk`], which keeps the frame's bytes. A painted
    /// update part is applied: the update's last part gives
    /// [`SessionFrame::Painted`], and an earlier part gives `None`. A binary
    /// frame of a kind this build does not have gives `None` and changes
    /// nothing.
    ///
    /// # Errors
    /// [`IpcError::MalformedFrame`] naming why a binary frame does not decode,
    /// or the [`PaintedUpdateError`] of a part that does not apply. The
    /// receiver holds no usable state after an error.
    pub fn accept_session_frame<Message>(
        &mut self,
        received_frame: ReceivedFrame<Message>,
    ) -> Result<Option<SessionFrame<Message>>, IpcError> {
        let frame_payload = match received_frame {
            ReceivedFrame::Message(message) => return Ok(Some(SessionFrame::Event(message))),
            ReceivedFrame::Binary(frame_payload) => frame_payload,
        };
        let painted_update_part = match decode_session_binary_frame(frame_payload) {
            Ok(SessionBinaryFrame::PaintedUpdatePart(painted_update_part)) => painted_update_part,
            Ok(SessionBinaryFrame::ImageChunk(image_chunk_frame)) => {
                return Ok(Some(SessionFrame::ImageChunk(image_chunk_frame)))
            }
            Ok(SessionBinaryFrame::Unknown { frame_kind }) => {
                tracing::debug!(frame_kind, "binary session frame this build does not have");
                return Ok(None);
            }
            Err(decode_error) => {
                return Err(IpcError::MalformedFrame {
                    error_detail: decode_error.to_string(),
                })
            }
        };
        match self.accept_painted_update_part(painted_update_part) {
            Ok(received_painted_frame) => {
                Ok(received_painted_frame.map(|received_painted_frame| {
                    SessionFrame::Painted(Box::new(received_painted_frame))
                }))
            }
            Err(painted_update_error) => Err(IpcError::MalformedFrame {
                error_detail: painted_update_error.to_string(),
            }),
        }
    }

    /// Apply one part of a painted update. Returns the applied update at its
    /// last part, and `None` before.
    ///
    /// # Errors
    /// The [`PaintedUpdateError`] that names the first rule the part breaks.
    /// The receiver holds no usable state after an error.
    pub fn accept_painted_update_part(
        &mut self,
        painted_update_part: PaintedUpdatePart,
    ) -> Result<Option<ReceivedPaintedFrame>, PaintedUpdateError> {
        let mut open_update = match (painted_update_part.update_opening, self.open_update.take()) {
            (Some(_), Some(_)) => return Err(PaintedUpdateError::UpdateAlreadyOpen),
            (None, None) => return Err(PaintedUpdateError::MissingUpdateOpening),
            (None, Some(open_update)) => open_update,
            (Some(update_opening), None) => {
                let expected_update_sequence = self.last_update_sequence + 1;
                if update_opening.update_sequence != expected_update_sequence {
                    return Err(PaintedUpdateError::UpdateOutOfSequence {
                        expected_update_sequence,
                        received_update_sequence: update_opening.update_sequence,
                    });
                }
                let painted_frame = match update_opening.painted_frame {
                    Some(painted_frame) => Arc::from(painted_frame),
                    None => Arc::clone(
                        self.held_painted_frame
                            .as_ref()
                            .ok_or(PaintedUpdateError::MissingStructure)?,
                    ),
                };
                OpenUpdate {
                    update_sequence: update_opening.update_sequence,
                    painted_frame,
                    staged_panes: Vec::new(),
                }
            }
        };
        for pane_section in painted_update_part.pane_sections {
            open_update.apply_pane_section(&self.held_pane_by_id, pane_section)?;
        }
        if !painted_update_part.is_last_part {
            self.open_update = Some(open_update);
            return Ok(None);
        }
        self.commit_update(open_update).map(Some)
    }

    /// Close `open_update`: check that its panes are complete and are the
    /// panes of its JSON part, hold its rows and JSON, and give it back.
    fn commit_update(
        &mut self,
        open_update: OpenUpdate,
    ) -> Result<ReceivedPaintedFrame, PaintedUpdateError> {
        if let Some(staged_pane) = open_update.staged_panes.last() {
            validate_staged_pane_row_count(staged_pane)?;
        }
        if open_update.staged_panes.len() != open_update.painted_frame.pane_snapshots.len() {
            return Err(PaintedUpdateError::PaneListMismatch);
        }
        let mut pane_views = Vec::with_capacity(open_update.staged_panes.len());
        let mut held_pane_by_id = HashMap::with_capacity(open_update.staged_panes.len());
        for staged_pane in open_update.staged_panes {
            let pane_header = staged_pane.pane_header;
            let retained_line_count = usize::try_from(pane_header.retained_line_count)
                .map_err(|_| PaintedUpdateError::CountTooLarge)?;
            let terminal_grid_view = match pane_header.pane_window {
                Some(pane_window) => {
                    held_pane_by_id.insert(
                        staged_pane.pane_id,
                        HeldPane {
                            column_count: pane_window.column_count,
                            row_snapshots: staged_pane.row_snapshots.clone(),
                        },
                    );
                    Some(GridView {
                        row_snapshots: staged_pane.row_snapshots,
                        column_count: pane_window.column_count,
                        view_row_offset: usize::try_from(pane_window.view_row_offset)
                            .map_err(|_| PaintedUpdateError::CountTooLarge)?,
                    })
                }
                None => None,
            };
            pane_views.push(ReceivedPaneView {
                pane_id: staged_pane.pane_id,
                cursor_snapshot: build_cursor_snapshot(pane_header.cursor),
                view_top_row_index: pane_header.view_top_row_index,
                retained_line_count,
                terminal_grid_view,
            });
        }
        self.held_pane_by_id = held_pane_by_id;
        self.held_painted_frame = Some(Arc::clone(&open_update.painted_frame));
        self.last_update_sequence = open_update.update_sequence;
        Ok(ReceivedPaintedFrame {
            update_sequence: open_update.update_sequence,
            painted_frame: open_update.painted_frame,
            pane_views,
        })
    }
}

impl OpenUpdate {
    /// Apply one section of this update, against the rows `held_pane_by_id`
    /// holds. A section with a header opens the next pane of the update's
    /// JSON part, after the pane before it holds all its rows. A section with
    /// no header goes on with the pane open.
    fn apply_pane_section(
        &mut self,
        held_pane_by_id: &HashMap<PaneId, HeldPane>,
        pane_section: PaneSection,
    ) -> Result<(), PaintedUpdateError> {
        let pane_id = pane_section.pane_id;
        let staged_pane = match pane_section.pane_header {
            Some(pane_header) => {
                if let Some(staged_pane) = self.staged_panes.last() {
                    validate_staged_pane_row_count(staged_pane)?;
                }
                let staged_pane_index = self.staged_panes.len();
                let is_next_frame_pane = self
                    .painted_frame
                    .pane_snapshots
                    .get(staged_pane_index)
                    .is_some_and(|frame_pane| frame_pane.pane_id == pane_id);
                if !is_next_frame_pane {
                    return Err(PaintedUpdateError::PaneListMismatch);
                }
                self.staged_panes.push(StagedPane {
                    pane_id,
                    pane_header,
                    row_snapshots: Vec::new(),
                });
                &mut self.staged_panes[staged_pane_index]
            }
            None => match self.staged_panes.last_mut() {
                Some(staged_pane) if staged_pane.pane_id == pane_id => staged_pane,
                _ => return Err(PaintedUpdateError::SectionContinuesNoPane { pane_id }),
            },
        };
        let Some(pane_window) = staged_pane.pane_header.pane_window else {
            if pane_section.row_instructions.is_empty() {
                return Ok(());
            }
            return Err(PaintedUpdateError::RowsWithoutWindow { pane_id });
        };
        let held_row_snapshots: &[Arc<RowSnapshot>] = if pane_window.is_reset {
            &[]
        } else {
            held_pane_by_id
                .get(&pane_id)
                .filter(|held_pane| held_pane.column_count == pane_window.column_count)
                .map_or(&[], |held_pane| held_pane.row_snapshots.as_slice())
        };
        for row_instruction in pane_section.row_instructions {
            let shown_row_index = staged_pane.row_snapshots.len();
            match row_instruction {
                RowInstruction::Keep { row_count } => {
                    let held_rows = held_row_snapshots
                        .get(shown_row_index..shown_row_index + usize::from(row_count))
                        .ok_or(PaintedUpdateError::HeldRowOutOfRange { pane_id })?;
                    staged_pane.row_snapshots.extend_from_slice(held_rows);
                }
                RowInstruction::Copy {
                    source_row_index,
                    row_count,
                } => {
                    let source_row_index = usize::from(source_row_index);
                    let held_rows = held_row_snapshots
                        .get(source_row_index..source_row_index + usize::from(row_count))
                        .ok_or(PaintedUpdateError::HeldRowOutOfRange { pane_id })?;
                    staged_pane.row_snapshots.extend_from_slice(held_rows);
                }
                RowInstruction::New { encoded_row_bytes } => {
                    let row_snapshot = RowSnapshot::from_encoded_row_bytes(
                        encoded_row_bytes,
                        pane_window.column_count,
                    )
                    .map_err(|row_decode_error| {
                        PaintedUpdateError::UndecodableRow {
                            pane_id,
                            row_decode_error,
                        }
                    })?;
                    staged_pane.row_snapshots.push(Arc::new(row_snapshot));
                }
            }
            if staged_pane.row_snapshots.len() > usize::from(pane_window.row_count) {
                return Err(PaintedUpdateError::PaneRowCountMismatch {
                    pane_id,
                    expected_row_count: pane_window.row_count,
                    received_row_count: staged_pane.row_snapshots.len(),
                });
            }
        }
        Ok(())
    }
}

/// Check that `staged_pane` holds exactly its window's rows: none for a pane
/// that shows no window.
fn validate_staged_pane_row_count(staged_pane: &StagedPane) -> Result<(), PaintedUpdateError> {
    let expected_row_count = staged_pane
        .pane_header
        .pane_window
        .map_or(0, |pane_window| pane_window.row_count);
    if staged_pane.row_snapshots.len() != usize::from(expected_row_count) {
        return Err(PaintedUpdateError::PaneRowCountMismatch {
            pane_id: staged_pane.pane_id,
            expected_row_count,
            received_row_count: staged_pane.row_snapshots.len(),
        });
    }
    Ok(())
}

/// The renderer's cursor for `pane_cursor`.
fn build_cursor_snapshot(pane_cursor: PaneCursor) -> CursorSnapshot {
    CursorSnapshot {
        row_index: pane_cursor.row_index,
        column_index: pane_cursor.column_index,
        is_visible: pane_cursor.is_visible,
        is_blinking: pane_cursor.is_blinking,
        shape: pane_cursor
            .shape
            .map(|pane_cursor_shape| match pane_cursor_shape {
                PaneCursorShape::Block => CursorShape::Block,
                PaneCursorShape::Underline => CursorShape::Underline,
                PaneCursorShape::Bar => CursorShape::Bar,
            }),
    }
}

#[cfg(test)]
mod tests;
