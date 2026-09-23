//! The frame reader: turning the [`PaintedFrame`](koshi_ipc::frame::PaintedFrame)
//! a session sends into the
//! [`RenderSnapshot`](koshi_renderer::snapshot::RenderSnapshot) this process
//! paints.
//!
//! [`build_render_snapshot`](crate::attach::paint::build_render_snapshot) is the inverse of
//! [`wire_frame`](koshi_runtime::runtime::frame::wire_frame), with the four
//! names the answering session chose filtered on the way in. The session, tab,
//! slot, tab-bar and client parts already hold shared types, so they copy
//! straight back. Each pane's cells are rebuilt: every
//! [`FrameRow`](koshi_ipc::frame::FrameRow) expands its runs back into cells,
//! each cell becomes a [`Cell`](koshi_terminal::grid::state::Cell), and the rows
//! become one [`Grid`](koshi_terminal::grid::state::Grid) behind an
//! [`Arc`](std::sync::Arc).
//!
//! Image placements arrive in the painted frame without RGBA. `ImageCache`
//! keeps complete records by their connection-local content identity and
//! rebuilds the newest snapshot after all required image chunks finish.
//!
//! A run travels once and expands back into as many cells as it stood for: a
//! blank 80-column row arrives as one run with `count: 80` and rebuilds into 80
//! blank cells.
//!
//! The session name, the active tab's name, every tab-bar entry's name and
//! every pane title pass through
//! [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text). This
//! process paints those four into its own terminal and puts two of them inside
//! an `OSC 0` window-title sequence, so a control character in one of them
//! would reach the terminal as a control character. A pane's cells are not
//! filtered: they are the pane's screen, and every byte in them is already a
//! grid cell.
//!
//! Plugin UI does not travel, so every frame read here carries the default
//! [`PluginUiSnapshot`](koshi_renderer::snapshot::PluginUiSnapshot).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use koshi_core::lock::LockMode;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::frame::{
    FrameCell, FrameColor, FrameCursorShape, FrameGraphicsProtocol, FrameImageAction,
    FrameImageChunk, FrameImageDimension, FrameImageDisplay, FrameImagePlacement,
    FrameImageRecordHeader, FrameImageTransfer, FramePane, FrameRow, FrameRowEnd,
    FrameSixelBackground, FrameSlot, FrameStyle, FrameTabMeta, FrameUnderline, FrameWindow,
    PaintedFrame, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT, MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT,
    MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_ipc::placement::PanePlacementSnapshot;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot,
    PlacementClientSnapshot, PlacementPaneSnapshot, PlacementSnapshot, PlacementTabSnapshot,
    PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::state::{Cell, Grid, RowEnd};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};

#[cfg(test)]
mod tests;

/// Turn one frame read off the event stream into the snapshot the renderer
/// draws, with every image placement marked unavailable.
///
/// The session name, the active tab's name, every tab-bar entry's name and
/// every pane title are filtered by [`sanitize_reported_text`]. A frame naming
/// its session `"dev\u{7}"` reads back naming it `"dev"`.
#[must_use]
pub fn build_render_snapshot(painted_frame: &PaintedFrame) -> RenderSnapshot {
    build_render_snapshot_with_images(painted_frame, &HashMap::new())
}

/// Turn one frame into a render snapshot using image records already received.
fn build_render_snapshot_with_images(
    painted_frame: &PaintedFrame,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> RenderSnapshot {
    let active_tab_snapshot = &painted_frame.session_snapshot.active_tab_snapshot;
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: painted_frame.session_snapshot.session_id,
            session_revision: painted_frame.session_snapshot.session_revision,
            session_name: sanitize_reported_text(&painted_frame.session_snapshot.session_name),
            active_tab_snapshot: TabSnapshot {
                tab_id: active_tab_snapshot.tab_id,
                tab_name: sanitize_reported_text(&active_tab_snapshot.tab_name),
                pane_slots: active_tab_snapshot
                    .pane_slots
                    .iter()
                    .map(build_pane_slot)
                    .collect(),
                effective_cell_size: active_tab_snapshot.effective_cell_size,
                stack_headers: active_tab_snapshot.stack_headers.clone(),
                layout_mode: active_tab_snapshot.layout_mode,
                are_all_panes_suppressed: active_tab_snapshot.is_every_pane_suppressed,
                gap_cell_count: active_tab_snapshot.gap_cell_count,
            },
            tabs_metadata: painted_frame
                .session_snapshot
                .tab_snapshots
                .iter()
                .map(build_tab_metadata)
                .collect(),
        },
        pane_snapshots: painted_frame
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| build_pane_snapshot(pane_snapshot, image_record_by_content_id))
            .collect(),
        client_snapshot: ClientSnapshot {
            client_id: painted_frame.client_snapshot.client_id,
            client_revision: painted_frame.client_snapshot.client_revision,
            viewport_size: painted_frame.client_snapshot.viewport_size,
            active_tab_id: painted_frame.client_snapshot.active_tab_id,
            focused_pane_id: painted_frame.client_snapshot.focused_pane_id,
            lock_mode: painted_frame.client_snapshot.lock_mode,
            is_mouse_selection_enabled: painted_frame.client_snapshot.is_mouse_selection_enabled,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// Build the renderer-owned placement preview from the bounded wire snapshot.
fn build_placement_render_snapshot(
    placement_snapshot: &PanePlacementSnapshot,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> Option<PlacementSnapshot> {
    let build_tab_snapshot = |tab_snapshot: &koshi_ipc::placement::PanePlacementTabSnapshot| {
        let pane_snapshots = tab_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| {
                let image_placement_snapshots = pane_snapshot
                    .image_placement_snapshots
                    .iter()
                    .map(|image_placement| {
                        build_image_placement_snapshot(image_placement, image_record_by_content_id)
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(PlacementPaneSnapshot {
                    pane_id: pane_snapshot.pane_id,
                    terminal_grid_view: pane_snapshot.terminal_window.as_ref().map(build_grid_view),
                    image_placement_snapshots,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(PlacementTabSnapshot {
            layout_tree: tab_snapshot.layout_tree.clone(),
            tab_snapshot: TabSnapshot {
                tab_id: tab_snapshot.tab_id,
                tab_name: sanitize_reported_text(&tab_snapshot.tab_name),
                pane_slots: tab_snapshot
                    .pane_slots
                    .iter()
                    .map(build_pane_slot)
                    .collect(),
                effective_cell_size: tab_snapshot.effective_cell_size,
                stack_headers: tab_snapshot.stack_headers.clone(),
                layout_mode: tab_snapshot.layout_mode,
                are_all_panes_suppressed: tab_snapshot.is_every_pane_suppressed,
                gap_cell_count: tab_snapshot.gap_cell_count,
            },
            pane_snapshots,
        })
    };
    let source_tab_snapshot = build_tab_snapshot(&placement_snapshot.source_tab_snapshot)?;
    let destination_tab_snapshot = match placement_snapshot.destination_tab_snapshot.as_ref() {
        Some(destination_tab_snapshot) => Some(build_tab_snapshot(destination_tab_snapshot)?),
        None => None,
    };
    Some(PlacementSnapshot {
        session_id: placement_snapshot.session_id,
        source_pane_id: placement_snapshot.source_pane_id,
        source_tab_id: placement_snapshot.source_tab_id,
        destination_tab_id: placement_snapshot.destination_tab_id,
        session_placement_revision: placement_snapshot.session_placement_revision,
        client_placement_revision: placement_snapshot.client_placement_revision,
        source_tab_snapshot,
        destination_tab_snapshot,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: ClientSnapshot {
                client_id: placement_snapshot.client_snapshot.client_id,
                client_revision: placement_snapshot.client_placement_revision,
                viewport_size: placement_snapshot.client_snapshot.viewport_size,
                active_tab_id: placement_snapshot.client_snapshot.active_tab_id,
                focused_pane_id: placement_snapshot.client_snapshot.focused_pane_id,
                lock_mode: LockMode::Normal,
                is_mouse_selection_enabled: false,
            },
            reported_pane_area: placement_snapshot.client_snapshot.pane_area,
        },
        pane_sizing: koshi_layout::solver::PaneSizing {
            minimum_size: placement_snapshot.pane_sizing.minimum_size,
            gap_cell_count: placement_snapshot.pane_sizing.gap_cell_count,
        },
    })
}

/// Image records retained by one attached client connection.
pub(crate) struct ImageCache {
    /// Complete records, keyed by identities in painted frames.
    image_record_by_content_id: HashMap<u64, Arc<ImageRecord>>,
    /// Total RGBA bytes retained in `image_record_by_content_id`.
    retained_image_byte_count: u64,
    /// The newest painted frame, retained while records arrive.
    painted_frame: Option<Box<PaintedFrame>>,
    /// The newest placement preview, retained while its records arrive.
    placement_snapshot: Option<Box<PanePlacementSnapshot>>,
    /// Record identities the newest painted frame still needs.
    missing_painted_image_content_ids: HashSet<u64>,
    /// Record identities the newest placement preview still needs.
    missing_placement_image_content_ids: HashSet<u64>,
    /// Image transfers accepted after the newest painted frame.
    image_transfer_count: usize,
    /// The record whose chunks are currently arriving.
    pending_image_transfer: Option<PendingImageTransfer>,
    /// Whether the next image transfers belong to a rejected placement snapshot.
    is_ignoring_stale_image_transfers: bool,
}

/// One image transfer and the bytes received for it.
struct PendingImageTransfer {
    image_transfer: FrameImageTransfer,
    rgba_bytes: Vec<u8>,
    received_byte_count: u64,
    is_ignored: bool,
}

/// A malformed or incomplete image transfer stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageAssemblyError {
    /// Two placements in one pane use one terminal-local identity.
    DuplicatePlacement,
    /// More image transfers followed one painted frame than the batch limit allows.
    TransferCountExceedsFrame,
    /// A transfer does not belong to the newest painted frame.
    UnknownTransfer {
        /// The image content identity named by the transfer.
        image_content_id: u64,
    },
    /// Another image transfer is still open.
    TransferAlreadyOpen,
    /// A complete image record was sent again.
    TransferAlreadyComplete {
        /// The image content identity already retained by the cache.
        image_content_id: u64,
    },
    /// A transfer byte length cannot be allocated by this process.
    ByteLengthDoesNotFit,
    /// No painted frame is available for the transfer.
    MissingBaseFrame,
    /// A chunk does not continue at the expected raw-byte offset.
    WrongOffset {
        /// The transfer identity.
        image_transfer_id: u64,
        /// The offset the receiver expects.
        expected_byte_offset: u64,
        /// The offset the chunk names.
        actual_byte_offset: u64,
    },
    /// A chunk exceeds the declared image byte length.
    ChunkExceedsImage,
    /// The chunk's final marker does not match its ending offset.
    FinalMarkerMismatch,
    /// A chunk is larger than the event limit.
    ChunkTooLarge,
    /// The retained and incoming image bytes exceed the connection limit.
    TransferBytesExceedFrame,
    /// A transfer's declared length does not match its pixel dimensions.
    InvalidTransferLength,
    /// A placement or its complete record cannot become valid render state.
    InvalidPlacement,
}

impl fmt::Display for ImageAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePlacement => {
                formatter.write_str("image placement identity is repeated in one pane")
            }
            Self::TransferCountExceedsFrame => {
                formatter.write_str("painted frame exceeds the image-transfer batch limit")
            }
            Self::UnknownTransfer { image_content_id } => {
                write!(
                    formatter,
                    "image transfer identity {image_content_id} is not needed"
                )
            }
            Self::TransferAlreadyOpen => {
                formatter.write_str("another image transfer is still open")
            }
            Self::TransferAlreadyComplete { image_content_id } => {
                write!(
                    formatter,
                    "image transfer identity {image_content_id} is already complete"
                )
            }
            Self::ByteLengthDoesNotFit => {
                formatter.write_str("image transfer byte length cannot fit this process")
            }
            Self::MissingBaseFrame => formatter.write_str("image transfer has no painted frame"),
            Self::WrongOffset {
                image_transfer_id,
                expected_byte_offset,
                actual_byte_offset,
            } => write!(
                formatter,
                "image transfer {image_transfer_id} expects offset {expected_byte_offset}, got {actual_byte_offset}"
            ),
            Self::ChunkExceedsImage => formatter.write_str("image chunk exceeds its image"),
            Self::FinalMarkerMismatch => {
                formatter.write_str("image chunk final marker does not match its length")
            }
            Self::ChunkTooLarge => formatter.write_str("image chunk exceeds its event limit"),
            Self::TransferBytesExceedFrame => {
                formatter.write_str("image records exceed the connection byte limit")
            }
            Self::InvalidTransferLength => {
                formatter.write_str("image transfer length does not match its dimensions")
            }
            Self::InvalidPlacement => {
                formatter.write_str("image transfer cannot become a render placement")
            }
        }
    }
}

impl std::error::Error for ImageAssemblyError {}

impl ImageCache {
    /// Build an empty image cache for one connection.
    pub(crate) fn new() -> Self {
        Self {
            image_record_by_content_id: HashMap::new(),
            retained_image_byte_count: 0,
            painted_frame: None,
            placement_snapshot: None,
            missing_painted_image_content_ids: HashSet::new(),
            missing_placement_image_content_ids: HashSet::new(),
            image_transfer_count: 0,
            pending_image_transfer: None,
            is_ignoring_stale_image_transfers: false,
        }
    }

    /// Ignore image transfers that follow a rejected placement snapshot.
    pub(crate) fn ignore_stale_placement_image_transfers(&mut self) {
        self.is_ignoring_stale_image_transfers = true;
    }

    /// Discard every connection-local image record and incomplete transfer.
    pub(crate) fn clear_image_cache(&mut self) {
        self.image_record_by_content_id.clear();
        self.retained_image_byte_count = 0;
        self.painted_frame = None;
        self.placement_snapshot = None;
        self.missing_painted_image_content_ids.clear();
        self.missing_placement_image_content_ids.clear();
        self.image_transfer_count = 0;
        self.pending_image_transfer = None;
        self.is_ignoring_stale_image_transfers = false;
    }

    /// Adopt a painted frame and return it when every required image is complete.
    pub(crate) fn adopt_painted_frame(
        &mut self,
        painted_frame: Box<PaintedFrame>,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        self.is_ignoring_stale_image_transfers = false;
        let placement_count = painted_frame
            .pane_snapshots
            .iter()
            .try_fold(0usize, |placement_count, pane_snapshot| {
                placement_count.checked_add(pane_snapshot.image_placement_snapshots.len())
            })
            .ok_or(ImageAssemblyError::TransferCountExceedsFrame)?;
        let mut required_image_content_ids = HashSet::new();
        let mut placement_key_set = HashSet::new();
        required_image_content_ids
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        placement_key_set
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        for pane_snapshot in &painted_frame.pane_snapshots {
            for image_placement in &pane_snapshot.image_placement_snapshots {
                if !placement_key_set.insert((pane_snapshot.pane_id, image_placement.placement_id))
                {
                    return Err(ImageAssemblyError::DuplicatePlacement);
                }
                let is_valid = image_placement_with_record(
                    image_placement,
                    self.image_record_by_content_id
                        .get(&image_placement.image_content_id),
                )
                .is_some();
                if !is_valid {
                    return Err(ImageAssemblyError::InvalidPlacement);
                }
                if image_placement.is_available {
                    required_image_content_ids.insert(image_placement.image_content_id);
                }
            }
        }

        let mut required_placement_image_content_ids = HashSet::new();
        if let Some(placement_snapshot) = &self.placement_snapshot {
            for image_placement in list_placement_image_snapshots(placement_snapshot) {
                if image_placement.is_available {
                    required_placement_image_content_ids.insert(image_placement.image_content_id);
                }
            }
        }
        let mut retained_image_content_ids = required_image_content_ids.clone();
        retained_image_content_ids.extend(&required_placement_image_content_ids);
        self.image_record_by_content_id
            .retain(|image_content_id, _| retained_image_content_ids.contains(image_content_id));
        self.retained_image_byte_count =
            compute_retained_image_byte_count(&self.image_record_by_content_id)?;
        self.missing_painted_image_content_ids = required_image_content_ids
            .into_iter()
            .filter(|image_content_id| {
                !self
                    .image_record_by_content_id
                    .contains_key(image_content_id)
            })
            .collect();
        self.missing_placement_image_content_ids = required_placement_image_content_ids
            .into_iter()
            .filter(|image_content_id| {
                !self
                    .image_record_by_content_id
                    .contains_key(image_content_id)
            })
            .collect();
        self.pending_image_transfer = None;
        self.image_transfer_count = 0;
        self.painted_frame = Some(painted_frame);
        if self.missing_painted_image_content_ids.is_empty() {
            self.build_render_snapshot().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Drop the retained placement preview while keeping the newest painted frame.
    pub(crate) fn clear_placement_snapshot(&mut self) -> Result<(), ImageAssemblyError> {
        self.placement_snapshot = None;
        self.missing_placement_image_content_ids.clear();
        let mut required_image_content_ids = HashSet::new();
        if let Some(painted_frame) = &self.painted_frame {
            for pane_snapshot in &painted_frame.pane_snapshots {
                for image_placement in &pane_snapshot.image_placement_snapshots {
                    if image_placement.is_available {
                        required_image_content_ids.insert(image_placement.image_content_id);
                    }
                }
            }
        }
        self.image_record_by_content_id
            .retain(|image_content_id, _| required_image_content_ids.contains(image_content_id));
        self.retained_image_byte_count =
            compute_retained_image_byte_count(&self.image_record_by_content_id)?;
        self.missing_painted_image_content_ids = required_image_content_ids
            .into_iter()
            .filter(|image_content_id| {
                !self
                    .image_record_by_content_id
                    .contains_key(image_content_id)
            })
            .collect();
        Ok(())
    }

    /// Build the renderer preview from the newest retained wire snapshot.
    pub(crate) fn build_placement_render_snapshot(&self) -> Option<PlacementSnapshot> {
        self.placement_snapshot
            .as_deref()
            .and_then(|placement_snapshot| {
                build_placement_render_snapshot(
                    placement_snapshot,
                    &self.image_record_by_content_id,
                )
            })
    }

    /// Retain a placement preview while its image records arrive.
    pub(crate) fn adopt_placement_snapshot(
        &mut self,
        placement_snapshot: Box<PanePlacementSnapshot>,
    ) -> Result<(), ImageAssemblyError> {
        self.is_ignoring_stale_image_transfers = false;
        placement_snapshot
            .validate()
            .map_err(|_| ImageAssemblyError::InvalidPlacement)?;
        let mut required_image_content_ids = HashSet::new();
        for placement in list_placement_image_snapshots(&placement_snapshot) {
            let is_valid = image_placement_with_record(
                placement,
                self.image_record_by_content_id
                    .get(&placement.image_content_id),
            )
            .is_some();
            if !is_valid {
                return Err(ImageAssemblyError::InvalidPlacement);
            }
            if placement.is_available {
                required_image_content_ids.insert(placement.image_content_id);
            }
        }
        let mut required_painted_image_content_ids = HashSet::new();
        if let Some(painted_frame) = &self.painted_frame {
            for pane_snapshot in &painted_frame.pane_snapshots {
                for image_placement in &pane_snapshot.image_placement_snapshots {
                    if image_placement.is_available {
                        required_painted_image_content_ids.insert(image_placement.image_content_id);
                    }
                }
            }
        }
        let mut retained_image_content_ids = required_image_content_ids.clone();
        retained_image_content_ids.extend(&required_painted_image_content_ids);
        self.image_record_by_content_id
            .retain(|image_content_id, _| retained_image_content_ids.contains(image_content_id));
        self.retained_image_byte_count =
            compute_retained_image_byte_count(&self.image_record_by_content_id)?;
        self.placement_snapshot = Some(placement_snapshot);
        self.missing_painted_image_content_ids = required_painted_image_content_ids
            .into_iter()
            .filter(|image_content_id| {
                !self
                    .image_record_by_content_id
                    .contains_key(image_content_id)
            })
            .collect();
        self.missing_placement_image_content_ids = required_image_content_ids
            .into_iter()
            .filter(|image_content_id| {
                !self
                    .image_record_by_content_id
                    .contains_key(image_content_id)
            })
            .collect();
        self.pending_image_transfer = None;
        self.image_transfer_count = 0;
        Ok(())
    }

    /// Start receiving one record needed by the newest painted frame or placement preview.
    pub(crate) fn start_image_transfer(
        &mut self,
        image_transfer: FrameImageTransfer,
    ) -> Result<(), ImageAssemblyError> {
        let is_ignored = self.is_ignoring_stale_image_transfers;
        if !is_ignored && self.painted_frame.is_none() && self.placement_snapshot.is_none() {
            return Err(ImageAssemblyError::MissingBaseFrame);
        }
        if self.pending_image_transfer.is_some() {
            return Err(ImageAssemblyError::TransferAlreadyOpen);
        }
        if !is_ignored
            && self
                .image_record_by_content_id
                .contains_key(&image_transfer.image_content_id)
        {
            return Err(ImageAssemblyError::TransferAlreadyComplete {
                image_content_id: image_transfer.image_content_id,
            });
        }
        if !is_ignored
            && !self
                .missing_painted_image_content_ids
                .contains(&image_transfer.image_content_id)
            && !self
                .missing_placement_image_content_ids
                .contains(&image_transfer.image_content_id)
        {
            return Err(ImageAssemblyError::UnknownTransfer {
                image_content_id: image_transfer.image_content_id,
            });
        }
        if self.image_transfer_count >= MAX_FRAME_IMAGE_TRANSFER_COUNT {
            return Err(ImageAssemblyError::TransferCountExceedsFrame);
        }
        let expected_image_byte_count = u64::from(image_transfer.image_record.pixel_width)
            .checked_mul(u64::from(image_transfer.image_record.pixel_height))
            .and_then(|pixel_count| pixel_count.checked_mul(4));
        if expected_image_byte_count != Some(image_transfer.image_byte_count)
            || image_transfer.image_byte_count == 0
        {
            return Err(ImageAssemblyError::InvalidTransferLength);
        }
        if is_ignored {
            if image_transfer.image_byte_count > MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT {
                return Err(ImageAssemblyError::TransferBytesExceedFrame);
            }
        } else {
            let total_image_byte_count = self
                .retained_image_byte_count
                .checked_add(image_transfer.image_byte_count)
                .ok_or(ImageAssemblyError::TransferBytesExceedFrame)?;
            if total_image_byte_count > MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT {
                return Err(ImageAssemblyError::TransferBytesExceedFrame);
            }
        }
        let mut rgba_bytes = Vec::new();
        if !is_ignored {
            let image_byte_capacity = usize::try_from(image_transfer.image_byte_count)
                .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
            rgba_bytes
                .try_reserve_exact(image_byte_capacity)
                .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        }
        self.pending_image_transfer = Some(PendingImageTransfer {
            image_transfer,
            rgba_bytes,
            received_byte_count: 0,
            is_ignored,
        });
        self.image_transfer_count += 1;
        Ok(())
    }

    /// Accept one chunk and return a complete frame when every missing record arrived.
    pub(crate) fn accept_image_chunk(
        &mut self,
        image_chunk: FrameImageChunk,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        let image_assembly_result = self.accept_image_chunk_inner(image_chunk);
        if image_assembly_result.is_err() {
            self.pending_image_transfer = None;
        }
        image_assembly_result
    }

    /// Validate and append one chunk to the open transfer.
    fn accept_image_chunk_inner(
        &mut self,
        image_chunk: FrameImageChunk,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        if image_chunk.chunk_bytes.is_empty() {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        if image_chunk.chunk_bytes.len() > MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT {
            return Err(ImageAssemblyError::ChunkTooLarge);
        }
        let pending_image = self
            .pending_image_transfer
            .as_mut()
            .filter(|pending_image| {
                pending_image.image_transfer.image_content_id == image_chunk.image_transfer_id
            })
            .ok_or(ImageAssemblyError::UnknownTransfer {
                image_content_id: image_chunk.image_transfer_id,
            })?;
        if image_chunk.byte_offset != pending_image.received_byte_count {
            return Err(ImageAssemblyError::WrongOffset {
                image_transfer_id: image_chunk.image_transfer_id,
                expected_byte_offset: pending_image.received_byte_count,
                actual_byte_offset: image_chunk.byte_offset,
            });
        }
        let chunk_byte_count = u64::try_from(image_chunk.chunk_bytes.len())
            .map_err(|_| ImageAssemblyError::ChunkExceedsImage)?;
        let image_byte_end = image_chunk
            .byte_offset
            .checked_add(chunk_byte_count)
            .ok_or(ImageAssemblyError::ChunkExceedsImage)?;
        if image_byte_end > pending_image.image_transfer.image_byte_count {
            return Err(ImageAssemblyError::ChunkExceedsImage);
        }
        if image_chunk.is_last != (image_byte_end == pending_image.image_transfer.image_byte_count)
        {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        if !pending_image.is_ignored {
            pending_image
                .rgba_bytes
                .extend_from_slice(&image_chunk.chunk_bytes);
        }
        pending_image.received_byte_count = image_byte_end;
        if image_byte_end != pending_image.image_transfer.image_byte_count {
            return Ok(None);
        }

        let completed_image_transfer =
            self.pending_image_transfer
                .take()
                .ok_or(ImageAssemblyError::UnknownTransfer {
                    image_content_id: image_chunk.image_transfer_id,
                })?;
        if completed_image_transfer.is_ignored {
            return Ok(None);
        }
        let image_content_id = completed_image_transfer.image_transfer.image_content_id;
        let image_record = Arc::new(build_image_record(
            &completed_image_transfer.image_transfer.image_record,
            completed_image_transfer.rgba_bytes,
        ));
        self.validate_image_record(image_content_id, &image_record)?;
        self.retained_image_byte_count = self
            .retained_image_byte_count
            .checked_add(completed_image_transfer.image_transfer.image_byte_count)
            .ok_or(ImageAssemblyError::TransferBytesExceedFrame)?;
        self.image_record_by_content_id
            .insert(image_content_id, image_record);
        self.missing_painted_image_content_ids
            .remove(&image_content_id);
        self.missing_placement_image_content_ids
            .remove(&image_content_id);
        if !self.missing_painted_image_content_ids.is_empty() {
            return Ok(None);
        }
        if self.placement_snapshot.is_some() {
            return Ok(None);
        }
        self.build_render_snapshot().map(Some)
    }

    /// Check a complete record against every placement that names it.
    fn validate_image_record(
        &self,
        image_content_id: u64,
        image_record: &Arc<ImageRecord>,
    ) -> Result<(), ImageAssemblyError> {
        let has_valid_painted_placement = self.painted_frame.as_ref().is_none_or(|painted_frame| {
            painted_frame.pane_snapshots.iter().all(|pane_snapshot| {
                pane_snapshot
                    .image_placement_snapshots
                    .iter()
                    .filter(|image_placement| {
                        image_placement.is_available
                            && image_placement.image_content_id == image_content_id
                    })
                    .all(|image_placement| {
                        image_placement_with_record(image_placement, Some(image_record)).is_some()
                    })
            })
        });
        let has_valid_placement_snapshot =
            self.placement_snapshot
                .as_ref()
                .is_none_or(|placement_snapshot| {
                    list_placement_image_snapshots(placement_snapshot)
                        .into_iter()
                        .filter(|image_placement| {
                            image_placement.is_available
                                && image_placement.image_content_id == image_content_id
                        })
                        .all(|image_placement| {
                            image_placement_with_record(image_placement, Some(image_record))
                                .is_some()
                        })
                });
        if has_valid_painted_placement && has_valid_placement_snapshot {
            Ok(())
        } else {
            Err(ImageAssemblyError::InvalidPlacement)
        }
    }

    /// Rebuild the newest painted frame with each available cached record.
    fn build_render_snapshot(&self) -> Result<RenderSnapshot, ImageAssemblyError> {
        self.painted_frame
            .as_deref()
            .map(|painted_frame| {
                build_render_snapshot_with_images(painted_frame, &self.image_record_by_content_id)
            })
            .ok_or(ImageAssemblyError::MissingBaseFrame)
    }
}

fn list_placement_image_snapshots(
    placement_snapshot: &PanePlacementSnapshot,
) -> Vec<&FrameImagePlacement> {
    let mut image_placement_snapshots = Vec::new();
    for placement_tab_snapshot in [
        Some(&placement_snapshot.source_tab_snapshot),
        placement_snapshot.destination_tab_snapshot.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        for pane_snapshot in &placement_tab_snapshot.pane_snapshots {
            image_placement_snapshots.extend(&pane_snapshot.image_placement_snapshots);
        }
    }
    image_placement_snapshots
}

/// Count the RGBA bytes in complete retained image records.
fn compute_retained_image_byte_count(
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> Result<u64, ImageAssemblyError> {
    image_record_by_content_id
        .values()
        .try_fold(0u64, |retained_image_byte_count, image_record| {
            let image_byte_count = u64::try_from(image_record.image.rgba_bytes.len())
                .map_err(|_| ImageAssemblyError::TransferBytesExceedFrame)?;
            retained_image_byte_count
                .checked_add(image_byte_count)
                .ok_or(ImageAssemblyError::TransferBytesExceedFrame)
        })
}

/// One solved pane placement, as the renderer reads it.
fn build_pane_slot(frame_slot: &FrameSlot) -> PaneSlot {
    PaneSlot {
        pane_id: frame_slot.pane_id,
        outer_rect: frame_slot.outer_rect,
        content_rect: frame_slot.content_rect,
        pane_kind: frame_slot.pane_kind,
        is_visible: frame_slot.is_visible,
        is_suppressed: frame_slot.is_suppressed,
        is_dead: frame_slot.is_dead,
    }
}

/// One tab-bar entry, as the renderer reads it. The name is filtered by
/// [`sanitize_reported_text`].
fn build_tab_metadata(frame_tab_metadata: &FrameTabMeta) -> TabMeta {
    TabMeta {
        tab_id: frame_tab_metadata.tab_id,
        tab_name: sanitize_reported_text(&frame_tab_metadata.tab_name),
        tab_index: frame_tab_metadata.tab_index,
        is_active: frame_tab_metadata.is_active,
    }
}

/// One pane's content, as the renderer reads it. A pane that sent no window has
/// no grid. The title is filtered by [`sanitize_reported_text`]; the cells are
/// not.
fn build_pane_snapshot(
    frame_pane: &FramePane,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> PaneSnapshot {
    PaneSnapshot {
        pane_id: frame_pane.pane_id,
        pane_title: frame_pane.pane_title.as_deref().map(sanitize_reported_text),
        cursor_snapshot: CursorSnapshot {
            row_index: frame_pane.cursor_snapshot.row_index,
            column_index: frame_pane.cursor_snapshot.column_index,
            is_visible: frame_pane.cursor_snapshot.is_visible,
            is_blinking: frame_pane.cursor_snapshot.is_blinking,
            shape: frame_pane
                .cursor_snapshot
                .shape
                .as_ref()
                .map(convert_frame_cursor_shape),
        },
        terminal_grid_view: frame_pane.terminal_window.as_ref().map(build_grid_view),
        image_placement_snapshots: frame_pane
            .image_placement_snapshots
            .iter()
            .filter_map(|frame_image_placement| {
                build_image_placement_snapshot(frame_image_placement, image_record_by_content_id)
            })
            .collect(),
        is_reverse_video: frame_pane.is_reverse_video,
        mouse_tracking: frame_pane.mouse_tracking,
        is_alternate_scroll_enabled: frame_pane.is_alt_scroll_enabled,
        is_on_alternate_screen: frame_pane.is_on_alt_screen,
        view_top_row_index: frame_pane.view_top_row_index,
        selection_spans: frame_pane
            .selection_spans
            .as_ref()
            .map(|frame_selection_spans| SelectionSpans {
                row_spans: frame_selection_spans.row_spans.clone(),
            }),
        has_selection: frame_pane.has_selection,
        scrollback_meta: ScrollbackMeta {
            is_truncated: frame_pane.scrollback_meta.is_truncated,
            retained_line_count: frame_pane.scrollback_meta.retained_line_count,
        },
    }
}

/// One wire image placement, with its cached record when the transfer completed.
fn build_image_placement_snapshot(
    frame_image_placement: &FrameImagePlacement,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> Option<ImagePlacementSnapshot> {
    image_placement_with_record(
        frame_image_placement,
        image_record_by_content_id.get(&frame_image_placement.image_content_id),
    )
}

fn image_placement_with_record(
    frame_image_placement: &FrameImagePlacement,
    cached_image_record: Option<&Arc<ImageRecord>>,
) -> Option<ImagePlacementSnapshot> {
    let image_placement_snapshot = if let Some(cached_image_record) =
        cached_image_record.filter(|_| frame_image_placement.is_available)
    {
        let image_record = if let Some(image_record_header) = &frame_image_placement.image_record {
            if image_record_header.pixel_width != cached_image_record.image.pixel_width
                || image_record_header.pixel_height != cached_image_record.image.pixel_height
            {
                return None;
            }
            let mut restored_image_record = cached_image_record.as_ref().clone();
            restored_image_record.protocol =
                convert_frame_graphics_protocol(image_record_header.protocol);
            restored_image_record.action =
                convert_frame_image_action(image_record_header.image_action);
            restored_image_record.display = build_image_display(&image_record_header.display);
            restored_image_record.anchor = image_record_header.anchor_cell;
            Arc::new(restored_image_record)
        } else {
            Arc::clone(cached_image_record)
        };
        ImagePlacementSnapshot::with_content_id(
            frame_image_placement.placement_id,
            frame_image_placement.image_content_id,
            image_record,
            frame_image_placement.anchor_cell,
            frame_image_placement.column_count,
            frame_image_placement.row_count,
        )?
    } else {
        ImagePlacementSnapshot::unavailable(
            frame_image_placement.placement_id,
            frame_image_placement.image_content_id,
            frame_image_placement.anchor_cell,
            frame_image_placement.column_count,
            frame_image_placement.row_count,
        )?
    };
    match frame_image_placement.cell_geometry {
        Some(cell_geometry) => image_placement_snapshot.with_cell_geometry(cell_geometry),
        None => Some(image_placement_snapshot),
    }
}

/// One complete image record rebuilt from transfer metadata and RGBA bytes.
fn build_image_record(
    image_record_header: &FrameImageRecordHeader,
    rgba_bytes: Vec<u8>,
) -> ImageRecord {
    ImageRecord {
        protocol: convert_frame_graphics_protocol(image_record_header.protocol),
        image: (DecodedImage {
            pixel_width: image_record_header.pixel_width,
            pixel_height: image_record_header.pixel_height,
            rgba_bytes,
        })
        .into(),
        animation: None,
        action: convert_frame_image_action(image_record_header.image_action),
        display: build_image_display(&image_record_header.display),
        anchor: image_record_header.anchor_cell,
    }
}

/// The source image protocol restored from the wire.
fn convert_frame_graphics_protocol(
    frame_graphics_protocol: FrameGraphicsProtocol,
) -> GraphicsProtocol {
    match frame_graphics_protocol {
        FrameGraphicsProtocol::Sixel => GraphicsProtocol::Sixel,
        FrameGraphicsProtocol::Kitty => GraphicsProtocol::Kitty,
        FrameGraphicsProtocol::Iterm2 => GraphicsProtocol::Iterm2,
    }
}

/// The wire image operation restored from the wire.
fn convert_frame_image_action(frame_image_action: FrameImageAction) -> ImageAction {
    match frame_image_action {
        FrameImageAction::Transmit => ImageAction::Transmit,
        FrameImageAction::Display => ImageAction::Display,
        FrameImageAction::TransmitAndDisplay => ImageAction::TransmitAndDisplay,
    }
}

/// One wire dimension restored as terminal image metadata.
fn convert_frame_image_dimension(frame_image_dimension: FrameImageDimension) -> ImageDimension {
    match frame_image_dimension {
        FrameImageDimension::Cells(dimension_value) => ImageDimension::Cells(dimension_value),
        FrameImageDimension::Pixels(dimension_value) => ImageDimension::Pixels(dimension_value),
        FrameImageDimension::Percent(dimension_value) => ImageDimension::Percent(dimension_value),
        FrameImageDimension::Auto => ImageDimension::Auto,
    }
}

/// One wire Sixel background rule restored as terminal image metadata.
fn convert_frame_sixel_background(frame_sixel_background: FrameSixelBackground) -> SixelBackground {
    match frame_sixel_background {
        FrameSixelBackground::Terminal => SixelBackground::Terminal,
        FrameSixelBackground::Preserve => SixelBackground::Preserve,
    }
}

/// Display metadata restored from the wire.
fn build_image_display(frame_image_display: &FrameImageDisplay) -> ImageDisplay {
    ImageDisplay {
        response_suppression_level: frame_image_display.response_suppression_level,
        requested_width: frame_image_display
            .requested_width
            .map(convert_frame_image_dimension),
        requested_height: frame_image_display
            .requested_height
            .map(convert_frame_image_dimension),
        is_aspect_ratio_preserved: frame_image_display.is_aspect_ratio_preserved,
        sixel_background: frame_image_display
            .sixel_background
            .map(convert_frame_sixel_background),
        image_id: frame_image_display.image_id,
        image_number: frame_image_display.image_number,
        placement_id: frame_image_display.placement_id,
        usage_hints: frame_image_display.usage_hints,
        is_unicode_placeholder: frame_image_display.is_unicode_placeholder,
        z_index: frame_image_display.z_index,
        relative_image_id: frame_image_display.relative_image_id,
        relative_placement_id: frame_image_display.relative_placement_id,
        relative_column_offset: frame_image_display.relative_column_offset,
        relative_row_offset: frame_image_display.relative_row_offset,
        requested_column_count: frame_image_display.requested_column_count,
        requested_row_count: frame_image_display.requested_row_count,
        source_pixel_offset_x: frame_image_display.source_pixel_offset_x,
        source_pixel_offset_y: frame_image_display.source_pixel_offset_y,
        cell_pixel_offset_x: frame_image_display.cell_pixel_offset_x,
        cell_pixel_offset_y: frame_image_display.cell_pixel_offset_y,
        should_move_cursor: frame_image_display.should_move_cursor,
    }
}

/// The pane's visible cells as one grid, plus how far its view is scrolled
/// back.
fn build_grid_view(frame_window: &FrameWindow) -> GridView {
    let terminal_grid_rows: Vec<Vec<Cell>> = frame_window
        .row_snapshots
        .iter()
        .map(build_grid_row)
        .collect();
    // `from_rows` starts every row `Hard`; each row the wire ends another way
    // is set back afterwards.
    let mut terminal_grid = Grid::from_rows(
        terminal_grid_rows,
        frame_window.column_count,
        Style::default(),
    );
    for (row_index, frame_row) in frame_window.row_snapshots.iter().enumerate() {
        let row_end = convert_frame_row_end(frame_row.row_end);
        if row_end != RowEnd::Hard {
            terminal_grid.set_row_end(u16::try_from(row_index).unwrap_or(u16::MAX), row_end);
        }
    }
    GridView {
        grid: Arc::new(terminal_grid),
        view_row_offset: frame_window.view_row_offset,
    }
}

/// One row's line-continuation state, read back from the wire.
fn convert_frame_row_end(frame_row_end: FrameRowEnd) -> RowEnd {
    match frame_row_end {
        FrameRowEnd::Hard => RowEnd::Hard,
        FrameRowEnd::Soft => RowEnd::Soft,
        FrameRowEnd::SoftWide => RowEnd::SoftWide,
    }
}

/// One row's runs, expanded back into cells: each run's cell is built once and
/// repeated `repeat_count` times. A run with `repeat_count: 80` yields 80
/// equal cells.
fn build_grid_row(frame_row: &FrameRow) -> Vec<Cell> {
    let expanded_cell_count = frame_row
        .cell_runs
        .iter()
        .map(|cell_run| usize::from(cell_run.repeat_count))
        .sum();
    let mut expanded_cells = Vec::with_capacity(expanded_cell_count);
    for cell_run in &frame_row.cell_runs {
        expanded_cells.extend(std::iter::repeat_n(
            build_terminal_cell(&cell_run.cell),
            usize::from(cell_run.repeat_count),
        ));
    }
    expanded_cells
}

/// One cell: its character, the rest of its grapheme cluster layered back on in
/// arrival order, its display width, and its style.
fn build_terminal_cell(frame_cell: &FrameCell) -> Cell {
    let mut terminal_cell = Cell::from_character(
        frame_cell.character,
        frame_cell.cell_width,
        build_terminal_style(&frame_cell.style),
    );
    for combining_character in &frame_cell.combining_characters {
        terminal_cell.push_combining(*combining_character);
    }
    terminal_cell
}

/// One cell's colors and text attributes.
fn build_terminal_style(frame_style: &FrameStyle) -> Style {
    let mut terminal_style = Style::default();
    terminal_style.set_foreground_color(convert_frame_color(&frame_style.foreground_color));
    terminal_style.set_background_color(convert_frame_color(&frame_style.background_color));
    terminal_style.set_underline_color(
        frame_style
            .underline_color
            .as_ref()
            .map(convert_frame_color),
    );
    terminal_style.set_bold(frame_style.text_attributes.is_bold);
    terminal_style.set_italic(frame_style.text_attributes.is_italic);
    terminal_style.set_underline(convert_frame_underline_style(
        &frame_style.text_attributes.underline_style,
    ));
    terminal_style.set_reverse(frame_style.text_attributes.is_reverse);
    terminal_style.set_faint(frame_style.text_attributes.is_faint);
    terminal_style.set_blink(frame_style.text_attributes.is_blinking);
    terminal_style.set_conceal(frame_style.text_attributes.is_concealed);
    terminal_style.set_strike(frame_style.text_attributes.is_struck_through);
    terminal_style.set_overline(frame_style.text_attributes.is_overlined);
    terminal_style
}

/// One foreground, background or underline color.
fn convert_frame_color(frame_color: &FrameColor) -> Color {
    match frame_color {
        FrameColor::Default => Color::Default,
        FrameColor::Indexed(color_index) => Color::Indexed(*color_index),
        FrameColor::Rgb(red, green, blue) => Color::Rgb(*red, *green, *blue),
    }
}

/// One cell's underline style.
fn convert_frame_underline_style(frame_underline: &FrameUnderline) -> UnderlineStyle {
    match frame_underline {
        FrameUnderline::None => UnderlineStyle::None,
        FrameUnderline::Single => UnderlineStyle::Single,
        FrameUnderline::Double => UnderlineStyle::Double,
        FrameUnderline::Curly => UnderlineStyle::Curly,
        FrameUnderline::Dotted => UnderlineStyle::Dotted,
        FrameUnderline::Dashed => UnderlineStyle::Dashed,
    }
}

/// The shape a pane asked its cursor to be drawn as.
fn convert_frame_cursor_shape(frame_cursor_shape: &FrameCursorShape) -> CursorShape {
    match frame_cursor_shape {
        FrameCursorShape::Block => CursorShape::Block,
        FrameCursorShape::Underline => CursorShape::Underline,
        FrameCursorShape::Bar => CursorShape::Bar,
    }
}
