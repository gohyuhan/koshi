//! The frame reader: turning the painted update a session sends, as
//! [`ReceivedPaintedFrame`](koshi_runtime::runtime::painted_update::ReceivedPaintedFrame)
//! gives it, into the
//! [`RenderSnapshot`](koshi_renderer::snapshot::RenderSnapshot) this process
//! paints.
//!
//! [`build_render_snapshot`](crate::attach::paint::build_render_snapshot) is the inverse of
//! [`build_wire_frame`](koshi_runtime::runtime::frame::build_wire_frame) and
//! the painted update together, with the four names the answering session
//! chose filtered on the way in. The session, tab, slot, tab-bar and client
//! parts already hold shared types, so they copy straight back. Each pane's
//! rows, cursor and scroll position come from the update's pane view, whose
//! rows the update already decoded.
//!
//! Image placements arrive in the painted frame without RGBA. `ImageCache`
//! keeps complete records by their connection-local content identity and
//! rebuilds the newest snapshot after all required image chunks finish.
//!
//! A placement preview carries each visible pane's rows in the row encoding
//! ([`koshi_terminal::row_encoding`]). `ImageCache` decodes them once, when it
//! adopts the preview, and refuses a row that does not decode or is not as
//! wide as its window.
//!
//! The session name, the active tab's name, every tab-bar entry's name and
//! every pane title pass through
//! [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text). This
//! process paints those four into its own terminal and puts two of them inside
//! an `OSC 0` window-title sequence. A pane's cells are not
//! filtered: they are the pane's screen, and every byte in them is already a
//! grid cell.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use koshi_core::ids::PaneId;
use koshi_core::lock::LockMode;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageDimension, FrameImageDisplay,
    FrameImagePlacement, FrameImageRecordHeader, FrameImageTransfer, FramePane,
    FrameSixelBackground, FrameSlot, FrameTabMetadata, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT,
    MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT, MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_ipc::image_chunk::ImageChunkFrame;
use koshi_ipc::placement::{PanePlacementSnapshot, PanePlacementSource, PlacementWindow};
use koshi_renderer::snapshot::{
    ClientSnapshot, FloatingPaneSlot, GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot,
    PlacementClientSnapshot, PlacementPaneSnapshot, PlacementSnapshot, PlacementSource,
    PlacementTabSnapshot, RenderSnapshot, ScrollbackMetadata, SelectionSpans, SessionSnapshot,
    TabMetadata, TabSnapshot,
};
use koshi_runtime::runtime::painted_update::{ReceivedPaintedFrame, ReceivedPaneView};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::row_snapshot::RowSnapshot;

#[cfg(test)]
pub(crate) mod tests;

/// Turn one painted update read off the event stream into the snapshot the
/// renderer draws, with every image placement marked unavailable.
///
/// The session name, the active tab's name, every tab-bar entry's name and
/// every pane title are filtered by [`sanitize_reported_text`]. A frame naming
/// its session `"dev\u{7}"` reads back naming it `"dev"`.
#[must_use]
pub fn build_render_snapshot(received_painted_frame: &ReceivedPaintedFrame) -> RenderSnapshot {
    build_render_snapshot_with_images(received_painted_frame, &HashMap::new())
}

/// Turn one painted update into a render snapshot using image records already
/// received. Pane `n` of the JSON part takes pane view `n`.
fn build_render_snapshot_with_images(
    received_painted_frame: &ReceivedPaintedFrame,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> RenderSnapshot {
    let painted_frame = &received_painted_frame.painted_frame;
    let active_tab_snapshot = &painted_frame.session_snapshot.active_tab_snapshot;
    RenderSnapshot {
        is_recovery_notice_visible: painted_frame.is_recovery_notice_visible,
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
                tab_size: active_tab_snapshot.tab_size,
                stack_headers: active_tab_snapshot.stack_headers.clone(),
                layout_mode: active_tab_snapshot.layout_mode,
                is_every_pane_suppressed: active_tab_snapshot.is_every_pane_suppressed,
                gap_cell_count: active_tab_snapshot.gap_cell_count,
            },
            tabs_metadata: painted_frame
                .session_snapshot
                .tab_snapshots
                .iter()
                .map(build_tab_metadata)
                .collect(),
            floating_pane_slots: painted_frame
                .session_snapshot
                .floating_pane_slots
                .iter()
                .map(|frame_floating_pane_slot| FloatingPaneSlot {
                    pane_id: frame_floating_pane_slot.pane_id,
                    outer_rect: frame_floating_pane_slot.outer_rect,
                    content_rect: frame_floating_pane_slot.content_rect,
                    is_pinned: frame_floating_pane_slot.is_pinned,
                })
                .collect(),
            floating_pane_ids: painted_frame.session_snapshot.floating_pane_ids.clone(),
        },
        pane_snapshots: painted_frame
            .pane_snapshots
            .iter()
            .zip(&received_painted_frame.pane_views)
            .map(|(frame_pane, pane_view)| {
                build_pane_snapshot(frame_pane, pane_view, image_record_by_content_id)
            })
            .collect(),
        client_snapshot: ClientSnapshot {
            client_id: painted_frame.client_snapshot.client_id,
            client_revision: painted_frame.client_snapshot.client_revision,
            viewport_size: painted_frame.client_snapshot.viewport_size,
            active_tab_id: painted_frame.client_snapshot.active_tab_id,
            focused_pane_id: painted_frame.client_snapshot.focused_pane_id,
            active_tab_focused_pane_id: painted_frame.client_snapshot.active_tab_focused_pane_id,
            lock_mode: painted_frame.client_snapshot.lock_mode,
            last_applied_lock_command_id: painted_frame
                .client_snapshot
                .last_applied_lock_command_id,
            last_applied_floating_pane_move_command_id: painted_frame
                .client_snapshot
                .last_applied_floating_pane_move_command_id,
            is_mouse_selection_enabled: painted_frame.client_snapshot.is_mouse_selection_enabled,
        },
    }
}

/// Build the renderer-owned placement preview from the bounded wire snapshot
/// and the rows `held_placement_snapshot` decoded for each of its panes.
fn build_placement_render_snapshot(
    held_placement_snapshot: &HeldPlacementSnapshot,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> Option<PlacementSnapshot> {
    let placement_snapshot = &held_placement_snapshot.placement_snapshot;
    let build_pane_snapshot = |pane_snapshot: &koshi_ipc::placement::PanePlacementPaneSnapshot| {
        let image_placement_snapshots = pane_snapshot
            .image_placement_snapshots
            .iter()
            .map(|image_placement| {
                build_image_placement_snapshot(image_placement, image_record_by_content_id)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(PlacementPaneSnapshot {
            pane_id: pane_snapshot.pane_id,
            terminal_grid_view: held_placement_snapshot
                .grid_view_by_pane_id
                .get(&pane_snapshot.pane_id)
                .cloned(),
            image_placement_snapshots,
        })
    };
    let build_tab_snapshot = |tab_snapshot: &koshi_ipc::placement::PanePlacementTabSnapshot| {
        let pane_snapshots = tab_snapshot
            .pane_snapshots
            .iter()
            .map(build_pane_snapshot)
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
                tab_size: tab_snapshot.tab_size,
                stack_headers: tab_snapshot.stack_headers.clone(),
                layout_mode: tab_snapshot.layout_mode,
                is_every_pane_suppressed: tab_snapshot.is_every_pane_suppressed,
                gap_cell_count: tab_snapshot.gap_cell_count,
            },
            pane_snapshots,
        })
    };
    let source = match &placement_snapshot.source {
        PanePlacementSource::Tiled(source_tab_snapshot) => {
            PlacementSource::Tiled(build_tab_snapshot(source_tab_snapshot)?)
        }
        PanePlacementSource::Floating(source_pane_snapshot) => {
            PlacementSource::Floating(build_pane_snapshot(source_pane_snapshot)?)
        }
    };
    let destination_tab_snapshot = match placement_snapshot.destination_tab_snapshot.as_ref() {
        Some(destination_tab_snapshot) => Some(build_tab_snapshot(destination_tab_snapshot)?),
        None => None,
    };
    Some(PlacementSnapshot {
        session_id: placement_snapshot.session_id,
        source_pane_id: placement_snapshot.source_pane_id,
        source,
        destination_tab_id: placement_snapshot.destination_tab_id,
        session_placement_revision: placement_snapshot.session_placement_revision,
        client_placement_revision: placement_snapshot.client_placement_revision,
        destination_tab_snapshot,
        client_snapshot: PlacementClientSnapshot {
            client_snapshot: ClientSnapshot {
                client_id: placement_snapshot.client_snapshot.client_id,
                client_revision: placement_snapshot.client_placement_revision,
                viewport_size: placement_snapshot.client_snapshot.viewport_size,
                active_tab_id: placement_snapshot.client_snapshot.active_tab_id,
                focused_pane_id: placement_snapshot.client_snapshot.focused_pane_id,
                active_tab_focused_pane_id: None,
                lock_mode: LockMode::Normal,
                last_applied_lock_command_id: None,
                last_applied_floating_pane_move_command_id: None,
                is_mouse_selection_enabled: false,
            },
            reported_pane_area: placement_snapshot.client_snapshot.pane_area,
        },
        pane_sizing: koshi_layout::solver::PaneSizing::from(placement_snapshot.pane_sizing),
    })
}

/// Image records retained by one attached client connection.
///
/// Each complete record stays until an `ImageContentReleased` names it or the
/// cache is cleared. A painted update or a placement preview keeps no record
/// and drops none.
pub(crate) struct ImageCache {
    /// Complete records, keyed by image content identity.
    image_record_by_content_id: HashMap<u64, Arc<ImageRecord>>,
    /// Total RGBA bytes retained in `image_record_by_content_id`.
    retained_image_byte_count: u64,
    /// The newest painted update, retained while records arrive.
    painted_frame: Option<Box<ReceivedPaintedFrame>>,
    /// The newest placement preview, retained while its records arrive.
    placement_snapshot: Option<Box<HeldPlacementSnapshot>>,
    /// Record identities the newest painted frame still needs.
    missing_painted_image_content_ids: HashSet<u64>,
    /// Record identities the newest placement preview still needs.
    missing_placement_image_content_ids: HashSet<u64>,
    /// Image transfers accepted since the newest painted update or placement
    /// preview, or since [`ImageCache::clear_image_transfer_count`].
    image_transfer_count: usize,
    /// The record whose chunks are currently arriving.
    pending_image_transfer: Option<PendingImageTransfer>,
}

/// A placement preview as the cache holds it: the request it answered, the
/// wire snapshot, and the rows of each pane that sent rows, decoded once. A
/// pane id appears at most once in a valid preview.
struct HeldPlacementSnapshot {
    request_id: u64,
    placement_snapshot: PanePlacementSnapshot,
    grid_view_by_pane_id: HashMap<PaneId, GridView>,
}

/// One image transfer and the bytes received for it.
struct PendingImageTransfer {
    image_transfer: FrameImageTransfer,
    rgba_bytes: Vec<u8>,
    received_byte_count: u64,
}

/// A malformed or incomplete image transfer stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageAssemblyError {
    /// Two placements in one pane use one terminal-local identity.
    DuplicatePlacement,
    /// More image transfers followed one painted frame than the batch limit allows.
    TransferCountExceedsFrame,
    /// A chunk names a transfer that is not open.
    UnknownTransfer {
        /// The image content identity named by the chunk.
        image_content_id: u64,
    },
    /// A release names a record the cache does not hold.
    UnknownRelease {
        /// The image content identity named by the release.
        image_content_id: u64,
    },
    /// A release names a record the newest painted frame or the held
    /// placement preview shows.
    ReleasedImageInUse {
        /// The image content identity named by the release.
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
                    "image chunk names transfer {image_content_id}, which is not open"
                )
            }
            Self::UnknownRelease { image_content_id } => {
                write!(
                    formatter,
                    "image release names record {image_content_id}, which is not held"
                )
            }
            Self::ReleasedImageInUse { image_content_id } => {
                write!(
                    formatter,
                    "image release names record {image_content_id}, which is still shown"
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
        }
    }

    /// Set the count of image transfers since the last painted update or
    /// placement preview to `0`, as adopting one does.
    pub(crate) fn clear_image_transfer_count(&mut self) {
        self.image_transfer_count = 0;
    }

    /// The request whose placement preview the cache holds, or `None` when it
    /// holds no preview.
    pub(crate) fn find_placement_request_id(&self) -> Option<u64> {
        self.placement_snapshot
            .as_ref()
            .map(|held_placement_snapshot| held_placement_snapshot.request_id)
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
    }

    /// Drop the record of each identity in `image_content_ids`.
    ///
    /// # Errors
    /// [`ImageAssemblyError::UnknownRelease`] for an identity the cache does
    /// not hold, and [`ImageAssemblyError::ReleasedImageInUse`] for one that
    /// an available placement of the newest painted frame or of the held
    /// placement preview names. The identities before it are dropped.
    pub(crate) fn release_image_contents(
        &mut self,
        image_content_ids: &[u64],
    ) -> Result<(), ImageAssemblyError> {
        let shown_image_content_ids: HashSet<u64> = self
            .list_held_image_placements()
            .filter(|image_placement| image_placement.is_available)
            .map(|image_placement| image_placement.image_content_id)
            .collect();
        for image_content_id in image_content_ids {
            if shown_image_content_ids.contains(image_content_id) {
                return Err(ImageAssemblyError::ReleasedImageInUse {
                    image_content_id: *image_content_id,
                });
            }
            let released_image_record = self
                .image_record_by_content_id
                .remove(image_content_id)
                .ok_or(ImageAssemblyError::UnknownRelease {
                    image_content_id: *image_content_id,
                })?;
            self.retained_image_byte_count = self
                .retained_image_byte_count
                .saturating_sub(compute_image_byte_count(&released_image_record));
        }
        Ok(())
    }

    /// Adopt a painted update and return its render snapshot when every
    /// required image is complete.
    pub(crate) fn adopt_painted_frame(
        &mut self,
        received_painted_frame: Box<ReceivedPaintedFrame>,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        let painted_frame = &received_painted_frame.painted_frame;
        let placement_count = painted_frame
            .pane_snapshots
            .iter()
            .try_fold(0usize, |placement_count, pane_snapshot| {
                placement_count.checked_add(pane_snapshot.image_placement_snapshots.len())
            })
            .ok_or(ImageAssemblyError::TransferCountExceedsFrame)?;
        let mut missing_image_content_ids = HashSet::new();
        let mut seen_placement_keys = HashSet::new();
        missing_image_content_ids
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        seen_placement_keys
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        for pane_snapshot in &painted_frame.pane_snapshots {
            for image_placement in &pane_snapshot.image_placement_snapshots {
                if !seen_placement_keys
                    .insert((pane_snapshot.pane_id, image_placement.placement_id))
                {
                    return Err(ImageAssemblyError::DuplicatePlacement);
                }
                let is_placement_valid = build_image_placement_snapshot_from_record(
                    image_placement,
                    self.image_record_by_content_id
                        .get(&image_placement.image_content_id),
                )
                .is_some();
                if !is_placement_valid {
                    return Err(ImageAssemblyError::InvalidPlacement);
                }
                if self.is_image_record_missing(image_placement) {
                    missing_image_content_ids.insert(image_placement.image_content_id);
                }
            }
        }

        self.missing_painted_image_content_ids = missing_image_content_ids;
        self.pending_image_transfer = None;
        self.image_transfer_count = 0;
        self.painted_frame = Some(received_painted_frame);
        if self.missing_painted_image_content_ids.is_empty() {
            Ok(self.build_render_snapshot())
        } else {
            Ok(None)
        }
    }

    /// Return the bytes of decoded pixels the cache holds: every retained
    /// record, and the RGBA bytes the open transfer reserved.
    pub(crate) fn compute_held_image_byte_count(&self) -> u64 {
        let pending_image_byte_count = self
            .pending_image_transfer
            .as_ref()
            .map_or(0, |pending_image_transfer| {
                pending_image_transfer.image_transfer.image_byte_count
            });
        self.retained_image_byte_count
            .saturating_add(pending_image_byte_count)
    }

    /// Drop the retained placement preview while keeping the newest painted
    /// frame and every record.
    pub(crate) fn clear_placement_snapshot(&mut self) {
        self.placement_snapshot = None;
        self.missing_placement_image_content_ids.clear();
    }

    /// Build the renderer preview from the newest retained wire snapshot.
    pub(crate) fn build_placement_render_snapshot(&self) -> Option<PlacementSnapshot> {
        self.placement_snapshot
            .as_deref()
            .and_then(|held_placement_snapshot| {
                build_placement_render_snapshot(
                    held_placement_snapshot,
                    &self.image_record_by_content_id,
                )
            })
    }

    /// Retain the placement preview that answers request `request_id` while
    /// its image records arrive, with each pane's rows decoded.
    ///
    /// # Errors
    /// [`ImageAssemblyError::InvalidPlacement`] for a preview that fails
    /// [`PanePlacementSnapshot::validate`], holds a row that does not decode or
    /// is not as wide as its window, or names an image placement no record
    /// can fill.
    pub(crate) fn adopt_placement_snapshot(
        &mut self,
        request_id: u64,
        placement_snapshot: Box<PanePlacementSnapshot>,
    ) -> Result<(), ImageAssemblyError> {
        placement_snapshot
            .validate()
            .map_err(|_| ImageAssemblyError::InvalidPlacement)?;
        let mut grid_view_by_pane_id = HashMap::new();
        for pane_snapshot in placement_snapshot.list_pane_snapshots() {
            if let Some(terminal_window) = &pane_snapshot.terminal_window {
                grid_view_by_pane_id.insert(
                    pane_snapshot.pane_id,
                    decode_placement_grid_view(terminal_window)?,
                );
            }
        }
        let mut missing_image_content_ids = HashSet::new();
        for image_placement in placement_snapshot.list_image_placements() {
            let is_placement_valid = build_image_placement_snapshot_from_record(
                image_placement,
                self.image_record_by_content_id
                    .get(&image_placement.image_content_id),
            )
            .is_some();
            if !is_placement_valid {
                return Err(ImageAssemblyError::InvalidPlacement);
            }
            if self.is_image_record_missing(image_placement) {
                missing_image_content_ids.insert(image_placement.image_content_id);
            }
        }
        self.placement_snapshot = Some(Box::new(HeldPlacementSnapshot {
            request_id,
            placement_snapshot: *placement_snapshot,
            grid_view_by_pane_id,
        }));
        self.missing_placement_image_content_ids = missing_image_content_ids;
        self.pending_image_transfer = None;
        self.image_transfer_count = 0;
        Ok(())
    }

    /// Start receiving one record. The cache keeps every record the session
    /// sends, whether or not the newest painted frame or placement preview
    /// names it.
    ///
    /// # Errors
    /// [`ImageAssemblyError::TransferAlreadyOpen`] while another transfer is
    /// open, [`ImageAssemblyError::TransferAlreadyComplete`] for a held
    /// identity, [`ImageAssemblyError::TransferCountExceedsFrame`] past
    /// [`MAX_FRAME_IMAGE_TRANSFER_COUNT`] transfers since the last painted
    /// update or placement preview,
    /// [`ImageAssemblyError::InvalidTransferLength`] for a byte count other
    /// than four per pixel, [`ImageAssemblyError::TransferBytesExceedFrame`]
    /// when the held records and this one pass
    /// [`MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT`], and
    /// [`ImageAssemblyError::ByteLengthDoesNotFit`] when its bytes cannot be
    /// allocated.
    pub(crate) fn start_image_transfer(
        &mut self,
        image_transfer: FrameImageTransfer,
    ) -> Result<(), ImageAssemblyError> {
        if self.pending_image_transfer.is_some() {
            return Err(ImageAssemblyError::TransferAlreadyOpen);
        }
        if self
            .image_record_by_content_id
            .contains_key(&image_transfer.image_content_id)
        {
            return Err(ImageAssemblyError::TransferAlreadyComplete {
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
        let total_image_byte_count = self
            .retained_image_byte_count
            .checked_add(image_transfer.image_byte_count)
            .ok_or(ImageAssemblyError::TransferBytesExceedFrame)?;
        if total_image_byte_count > MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT {
            return Err(ImageAssemblyError::TransferBytesExceedFrame);
        }
        let image_byte_capacity = usize::try_from(image_transfer.image_byte_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        let mut rgba_bytes = Vec::new();
        rgba_bytes
            .try_reserve_exact(image_byte_capacity)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        self.pending_image_transfer = Some(PendingImageTransfer {
            image_transfer,
            rgba_bytes,
            received_byte_count: 0,
        });
        self.image_transfer_count += 1;
        Ok(())
    }

    /// Accept one chunk, and return the newest painted frame when this chunk
    /// completed the last record it lacked. The chunk's bytes are copied once,
    /// onto the end of the open transfer's RGBA bytes.
    pub(crate) fn accept_image_chunk(
        &mut self,
        image_chunk_frame: &ImageChunkFrame,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        let image_assembly_result = self.append_image_chunk(image_chunk_frame);
        if image_assembly_result.is_err() {
            self.pending_image_transfer = None;
        }
        image_assembly_result
    }

    /// Validate and append one chunk to the open transfer.
    fn append_image_chunk(
        &mut self,
        image_chunk_frame: &ImageChunkFrame,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        let chunk_bytes = image_chunk_frame.get_chunk_bytes();
        if chunk_bytes.is_empty() {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        if chunk_bytes.len() > MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT {
            return Err(ImageAssemblyError::ChunkTooLarge);
        }
        let image_transfer_id = image_chunk_frame.get_image_transfer_id();
        let byte_offset = image_chunk_frame.get_byte_offset();
        let pending_image_transfer = self
            .pending_image_transfer
            .as_mut()
            .filter(|pending_image_transfer| {
                pending_image_transfer.image_transfer.image_content_id == image_transfer_id
            })
            .ok_or(ImageAssemblyError::UnknownTransfer {
                image_content_id: image_transfer_id,
            })?;
        if byte_offset != pending_image_transfer.received_byte_count {
            return Err(ImageAssemblyError::WrongOffset {
                image_transfer_id,
                expected_byte_offset: pending_image_transfer.received_byte_count,
                actual_byte_offset: byte_offset,
            });
        }
        let chunk_byte_count =
            u64::try_from(chunk_bytes.len()).map_err(|_| ImageAssemblyError::ChunkExceedsImage)?;
        let image_byte_end = byte_offset
            .checked_add(chunk_byte_count)
            .ok_or(ImageAssemblyError::ChunkExceedsImage)?;
        if image_byte_end > pending_image_transfer.image_transfer.image_byte_count {
            return Err(ImageAssemblyError::ChunkExceedsImage);
        }
        if image_chunk_frame.is_last_chunk()
            != (image_byte_end == pending_image_transfer.image_transfer.image_byte_count)
        {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        pending_image_transfer
            .rgba_bytes
            .extend_from_slice(chunk_bytes);
        pending_image_transfer.received_byte_count = image_byte_end;
        if image_byte_end != pending_image_transfer.image_transfer.image_byte_count {
            return Ok(None);
        }

        let completed_image_transfer = self
            .pending_image_transfer
            .take()
            .expect("the open transfer took the chunk above");
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
        let has_completed_painted_record = self
            .missing_painted_image_content_ids
            .remove(&image_content_id);
        self.missing_placement_image_content_ids
            .remove(&image_content_id);
        if !has_completed_painted_record || !self.missing_painted_image_content_ids.is_empty() {
            return Ok(None);
        }
        Ok(self.build_render_snapshot())
    }

    /// Check a complete record against every available placement of the
    /// newest painted frame and of the held placement preview that names it.
    fn validate_image_record(
        &self,
        image_content_id: u64,
        image_record: &Arc<ImageRecord>,
    ) -> Result<(), ImageAssemblyError> {
        let is_every_placement_valid = self
            .list_held_image_placements()
            .filter(|image_placement| {
                image_placement.is_available && image_placement.image_content_id == image_content_id
            })
            .all(|image_placement| {
                build_image_placement_snapshot_from_record(image_placement, Some(image_record))
                    .is_some()
            });
        if is_every_placement_valid {
            Ok(())
        } else {
            Err(ImageAssemblyError::InvalidPlacement)
        }
    }

    /// Every image placement of the newest painted frame, then every image
    /// placement of the held placement preview.
    fn list_held_image_placements(&self) -> impl Iterator<Item = &FrameImagePlacement> {
        let painted_image_placements = self
            .painted_frame
            .iter()
            .flat_map(|received_painted_frame| &received_painted_frame.painted_frame.pane_snapshots)
            .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots);
        let preview_image_placements =
            self.placement_snapshot
                .iter()
                .flat_map(|held_placement_snapshot| {
                    held_placement_snapshot
                        .placement_snapshot
                        .list_image_placements()
                });
        painted_image_placements.chain(preview_image_placements)
    }

    /// Whether `image_placement` is available and the cache holds no record of
    /// its image content.
    fn is_image_record_missing(&self, image_placement: &FrameImagePlacement) -> bool {
        image_placement.is_available
            && !self
                .image_record_by_content_id
                .contains_key(&image_placement.image_content_id)
    }

    /// Rebuild the newest painted update with each available cached record,
    /// or `None` when the cache holds no painted update.
    fn build_render_snapshot(&self) -> Option<RenderSnapshot> {
        self.painted_frame.as_deref().map(|received_painted_frame| {
            build_render_snapshot_with_images(
                received_painted_frame,
                &self.image_record_by_content_id,
            )
        })
    }
}

/// The RGBA byte count of `image_record`'s pixels.
fn compute_image_byte_count(image_record: &ImageRecord) -> u64 {
    u64::try_from(image_record.image.rgba_bytes.len()).unwrap_or(u64::MAX)
}

/// One solved pane placement, as the renderer reads it.
fn build_pane_slot(frame_slot: &FrameSlot) -> PaneSlot {
    PaneSlot {
        pane_id: frame_slot.pane_id,
        outer_rect: frame_slot.outer_rect,
        content_rect: frame_slot.content_rect,
        is_visible: frame_slot.is_visible,
        is_suppressed: frame_slot.is_suppressed,
    }
}

/// One tab-bar entry, as the renderer reads it. The name is filtered by
/// [`sanitize_reported_text`].
fn build_tab_metadata(frame_tab_metadata: &FrameTabMetadata) -> TabMetadata {
    TabMetadata {
        tab_id: frame_tab_metadata.tab_id,
        tab_name: sanitize_reported_text(&frame_tab_metadata.tab_name),
        tab_index: frame_tab_metadata.tab_index,
        is_active: frame_tab_metadata.is_active,
    }
}

/// One pane's content, as the renderer reads it: the JSON part's
/// `frame_pane`, and the cursor, scroll position and rows of `pane_view`. A
/// pane that sent no window has no grid. The title is filtered by
/// [`sanitize_reported_text`]; the cells are not.
fn build_pane_snapshot(
    frame_pane: &FramePane,
    pane_view: &ReceivedPaneView,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> PaneSnapshot {
    PaneSnapshot {
        pane_id: frame_pane.pane_id,
        pane_title: frame_pane.pane_title.as_deref().map(sanitize_reported_text),
        cursor_snapshot: pane_view.cursor_snapshot,
        terminal_grid_view: pane_view.terminal_grid_view.clone(),
        image_placement_snapshots: frame_pane
            .image_placement_snapshots
            .iter()
            .filter_map(|frame_image_placement| {
                build_image_placement_snapshot(frame_image_placement, image_record_by_content_id)
            })
            .collect(),
        is_reverse_video: frame_pane.is_reverse_video,
        mouse_tracking: frame_pane.mouse_tracking,
        is_alternate_scroll_enabled: frame_pane.is_alternate_scroll_enabled,
        is_on_alternate_screen: frame_pane.is_on_alt_screen,
        view_top_row_index: pane_view.view_top_row_index,
        selection_spans: frame_pane
            .selection_spans
            .as_ref()
            .map(|frame_selection_spans| SelectionSpans {
                row_spans: frame_selection_spans.row_spans.clone(),
            }),
        has_selection: frame_pane.has_selection,
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: pane_view.retained_line_count,
        },
    }
}

/// One wire image placement, with its cached record when the transfer completed.
fn build_image_placement_snapshot(
    frame_image_placement: &FrameImagePlacement,
    image_record_by_content_id: &HashMap<u64, Arc<ImageRecord>>,
) -> Option<ImagePlacementSnapshot> {
    build_image_placement_snapshot_from_record(
        frame_image_placement,
        image_record_by_content_id.get(&frame_image_placement.image_content_id),
    )
}

/// `frame_image_placement` as the renderer reads it. An available placement
/// with `cached_image_record` shows that record, with the protocol, action,
/// display and anchor of the placement's own record header when it carries
/// one. Any other placement is unavailable.
///
/// Returns `None` when the header's pixel size differs from the record's, when
/// [`ImagePlacementSnapshot::with_content_id`] or
/// [`ImagePlacementSnapshot::build_unavailable`] refuses the placement, or when
/// its cell geometry does not hold its full cell size
/// ([`ImagePlacementSnapshot::with_cell_geometry`]).
fn build_image_placement_snapshot_from_record(
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
        ImagePlacementSnapshot::build_unavailable(
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
        FrameImageDimension::Cells(dimension_amount) => ImageDimension::Cells(dimension_amount),
        FrameImageDimension::Pixels(dimension_amount) => ImageDimension::Pixels(dimension_amount),
        FrameImageDimension::Percent(dimension_amount) => ImageDimension::Percent(dimension_amount),
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

/// The rows of one pane of a placement preview, decoded. A preview view is
/// never scrolled back.
///
/// # Errors
/// [`ImageAssemblyError::InvalidPlacement`] for a row that does not decode,
/// or is not `terminal_window.column_count` cells wide.
fn decode_placement_grid_view(
    terminal_window: &PlacementWindow,
) -> Result<GridView, ImageAssemblyError> {
    let row_snapshots = terminal_window
        .encoded_rows
        .iter()
        .map(|encoded_row| {
            RowSnapshot::from_encoded_row_bytes(
                encoded_row.encoded_row_bytes.clone(),
                terminal_window.column_count,
            )
            .map(Arc::new)
            .map_err(|_| ImageAssemblyError::InvalidPlacement)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GridView {
        row_snapshots,
        column_count: terminal_window.column_count,
        view_row_offset: 0,
    })
}
