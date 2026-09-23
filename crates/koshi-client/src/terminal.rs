//! The outer terminal an attached client owns: the viewer built for it, the
//! thread that reads its input, and painting frames into it.
//!
//! Every item here belongs to one attached terminal. The session it is joined
//! to owns none of them.

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::attach::ViewerPaint;
use crate::Client;
use koshi_core::command::{PanePlacementAnchor, PanePlacementTarget};
use koshi_core::geometry::{PixelCellSize, Point, Rect as CoreRect, Size};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::KeySequence;
use koshi_input::host::{Event, WindowSize};
use koshi_input::keyboard::decode_key_event;
use koshi_input::mouse::decode_mouse;
use koshi_ipc::protocol::GraphicsCapabilities;
use koshi_iterm::{
    iterm_feature_string_supports_file, iterm_feature_string_supports_sixel,
    ITERM_CAPABILITIES_QUERY,
};
use koshi_kitty::{
    write_kitty_abort, write_kitty_delete_all, write_kitty_support_query, KittyOutputError,
    KITTY_QUERY_IMAGE_ID,
};
use koshi_layout::content::list_content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{place_pane_across_tabs, place_pane_within_tab, PlacementTarget};
use koshi_layout::solver::solve_layout_with_mode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, ImagePlacementSnapshot, KeymapHints, PaneSlot,
    PlacementPaneSnapshot, PlacementSnapshot, PlacementStatus, PlacementTabSnapshot,
    RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    build_image_cell_snapshot, build_image_paints, draw_grid_preview, draw_image_placeholders,
    get_cursor_position, get_cursor_style, render_frame_with_image_availability, ImageOutputKey,
    ImagePaint, ImagePlacementKey, ImageRenderMode, ImageSourceRect, PlacementPreviewImageKey,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_sixel::{PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY};
use koshi_terminal::graphics::ImageRecord;
use koshi_terminal::state::CursorShape;

use self::platform::{PlatformWaker, TerminalDevice};
use self::reader::InputReader;

mod image_output;
mod platform;
mod reader;

pub(crate) use self::image_output::{ImageCompatibility, ImageOutputKind, ImageOutputState};

const TERMINAL_QUERY_TIMEOUT_DURATION: Duration = Duration::from_millis(300);

/// Request a terminal's current cell dimensions in pixels.
const CELL_SIZE_QUERY_BYTES: &[u8] = b"\x1b[16t";

/// Enter the alternate screen and enable keyboard, mouse, and paste reports.
///
/// The keyboard push asks for flags `1|2|4|8|16`: disambiguate escape codes,
/// report event types, report alternate keys, report all keys as escape codes,
/// and report associated text. Every one of those reaches a pane through the
/// event Koshi sends, encoded for that pane's own flags.
///
/// Flags `8` and `16` together move ordinary typing into `CSI u` reports that
/// carry the text the key produced, so typing `å` reaches a pane as `å`. Flag
/// `8` alone would drop that text, and the specification defines `16` only
/// beside `8`.
///
/// A terminal that ignores the push keeps reporting legacy bytes, and every
/// key reaches a pane as it does today.
const APPLICATION_MODE_SETUP_BYTES: &[u8] =
    b"\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?2004h";

/// Ask which Kitty keyboard enhancements the terminal applied. The answer is
/// `ESC [ ? flags u`.
const KEYBOARD_ENHANCEMENT_QUERY_BYTES: &[u8] = b"\x1b[?u";

/// Disable paste, mouse, keyboard, and alternate-screen modes; restore cursor state.
const APPLICATION_MODE_CLEANUP_BYTES: &[u8] =
    b"\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q";

/// Paints a render snapshot into ratatui's frame buffer. Every field is handed
/// straight to [`koshi_renderer::render_frame_with_images`].
pub(crate) struct SnapshotWidget<'a> {
    /// The frame the session handed out.
    pub(crate) snapshot: &'a RenderSnapshot,
    /// The colors this viewer paints koshi's chrome in.
    pub(crate) theme: &'a Theme,
    /// The hint-bar data for the mode this viewer is in.
    pub(crate) hints: &'a KeymapHints,
    /// The multi-chord sequence this viewer has open.
    pub(crate) pending_key_sequence: Option<&'a KeySequence>,
    /// The pane this viewer's pointer is over, and where its tab strip sits.
    pub(crate) chrome: ViewerChrome,
    /// The region solve committed with the frame being painted.
    pub(crate) committed_regions: &'a CommittedRegions,
    /// The image mode selected for this outer terminal.
    pub(crate) image_mode: ImageRenderMode,
    /// The image placements whose native bytes are ready for this frame.
    pub(crate) available_image_placement_keys: Option<&'a [ImagePlacementKey]>,
    /// The placement-preview images whose native bytes are ready for this frame.
    pub(crate) prepared_preview_image_keys: Option<&'a [PlacementPreviewImageKey]>,
    /// The bounded read-only placement preview, if one is accepted.
    pub(crate) placement_snapshot: Option<&'a PlacementSnapshot>,
    /// The interpolated placement preview shown during local animation.
    pub(crate) placement_display_snapshot: Option<&'a PlacementSnapshot>,
    /// The target selected by the viewer's placement interaction.
    pub(crate) placement_target: Option<&'a PanePlacementTarget>,
    /// The statusline entry for the viewer's placement interaction.
    pub(crate) placement_status: Option<&'a PlacementStatus>,
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, render_area: Rect, render_buffer: &mut Buffer) {
        render_frame_with_image_availability(
            self.snapshot,
            self.committed_regions,
            self.theme,
            self.hints,
            self.pending_key_sequence,
            self.chrome,
            self.image_mode,
            self.available_image_placement_keys,
            self.placement_status,
            render_area,
            render_buffer,
        );
        if let Some(placement_display_snapshot) =
            self.placement_display_snapshot.or(self.placement_snapshot)
        {
            let placement_snapshot = self
                .placement_snapshot
                .unwrap_or(placement_display_snapshot);
            draw_placement_preview(
                placement_snapshot,
                placement_display_snapshot,
                self.placement_target,
                self.theme,
                self.image_mode,
                self.prepared_preview_image_keys,
                render_area,
                render_buffer,
            );
        }
    }
}

/// Draw the bounded source and destination placement views above the base frame.
#[allow(clippy::too_many_arguments)]
fn draw_placement_preview(
    placement_snapshot: &PlacementSnapshot,
    placement_display_snapshot: &PlacementSnapshot,
    placement_target: Option<&PanePlacementTarget>,
    theme: &Theme,
    image_mode: ImageRenderMode,
    prepared_preview_image_keys: Option<&[PlacementPreviewImageKey]>,
    render_area: Rect,
    render_buffer: &mut Buffer,
) {
    let Some(panel_rect) = compute_placement_preview_panel_rect(render_area) else {
        return;
    };
    let panel_style = Style::default()
        .fg(theme.accent_color)
        .bg(theme.bar_background_color);
    for row_index in panel_rect.y..panel_rect.y + panel_rect.height {
        render_buffer.set_string(
            panel_rect.x,
            row_index,
            " ".repeat(usize::from(panel_rect.width)),
            panel_style,
        );
    }
    draw_box_border(panel_rect, panel_style, render_buffer);
    render_buffer.set_string(
        panel_rect.x + 2,
        panel_rect.y,
        " placement preview ",
        panel_style,
    );

    let destination_base_snapshot = placement_snapshot
        .destination_tab_snapshot
        .as_ref()
        .unwrap_or(&placement_snapshot.source_tab_snapshot);
    let destination_snapshot = placement_display_snapshot
        .destination_tab_snapshot
        .as_ref()
        .unwrap_or(&placement_display_snapshot.source_tab_snapshot);

    let Some((source_rect, destination_rect)) = compute_placement_preview_tab_rects(render_area)
    else {
        return;
    };

    draw_placement_tab(
        &placement_display_snapshot.source_tab_snapshot,
        &placement_snapshot.source_tab_snapshot,
        source_rect,
        "source",
        placement_snapshot.source_pane_id,
        placement_target,
        0,
        theme,
        image_mode,
        prepared_preview_image_keys,
        render_buffer,
    );
    draw_placement_tab(
        destination_snapshot,
        destination_base_snapshot,
        destination_rect,
        if placement_snapshot.destination_tab_snapshot.is_some() {
            "destination"
        } else {
            "proposed"
        },
        placement_snapshot.source_pane_id,
        placement_target,
        1,
        theme,
        image_mode,
        prepared_preview_image_keys,
        render_buffer,
    );
}

/// Build the overlay cells used beneath native placement-preview images.
fn build_placement_composition_buffer(
    placement_snapshot: &PlacementSnapshot,
    placement_display_snapshot: &PlacementSnapshot,
    placement_target: Option<&PanePlacementTarget>,
    theme: &Theme,
    image_paints: &[ImagePaint],
    render_area: Rect,
) -> Option<(Buffer, Rect)> {
    let panel_rect = compute_placement_preview_panel_rect(render_area)?;
    let preview_image_keys = image_paints
        .iter()
        .filter_map(|image_paint| match image_paint.get_output_key() {
            ImageOutputKey::PlacementPreview(preview_image_key) => Some(preview_image_key),
            ImageOutputKey::Frame(_) => None,
        })
        .collect::<Vec<_>>();
    let mut render_buffer = Buffer::empty(render_area);
    draw_placement_preview(
        placement_snapshot,
        placement_display_snapshot,
        placement_target,
        theme,
        ImageRenderMode::Native,
        Some(&preview_image_keys),
        render_area,
        &mut render_buffer,
    );
    Some((render_buffer, panel_rect))
}

/// Return the bounded rectangle used by the placement preview overlay.
fn compute_placement_preview_panel_rect(render_area: Rect) -> Option<Rect> {
    let panel_width = render_area.width.saturating_sub(2).min(120);
    let panel_height = render_area.height.saturating_sub(2).min(30);
    (panel_width >= 30 && panel_height >= 8).then(|| {
        let panel_x = render_area.x + render_area.width.saturating_sub(panel_width) / 2;
        Rect::new(panel_x, render_area.y + 1, panel_width, panel_height)
    })
}

/// Return the two tab rectangles used by the placement preview overlay.
pub(crate) fn compute_placement_preview_tab_rects(render_area: Rect) -> Option<(Rect, Rect)> {
    let panel_rect = compute_placement_preview_panel_rect(render_area)?;
    let inner_rect = Rect::new(
        panel_rect.x + 1,
        panel_rect.y + 1,
        panel_rect.width.saturating_sub(2),
        panel_rect.height.saturating_sub(2),
    );
    let tab_gap = 1;
    let tab_width = inner_rect.width.saturating_sub(tab_gap) / 2;
    if tab_width < 12 {
        return None;
    }
    let tab_height = inner_rect.height;
    let source_rect = Rect::new(inner_rect.x, inner_rect.y, tab_width, tab_height);
    let destination_rect = Rect::new(
        inner_rect.x + tab_width + tab_gap,
        inner_rect.y,
        tab_width,
        tab_height,
    );
    Some((source_rect, destination_rect))
}

/// Return the layout rectangle inside one preview tab.
pub(crate) fn compute_placement_tab_layout_rect(tab_rect: Rect) -> Option<Rect> {
    (tab_rect.width >= 4 && tab_rect.height >= 5).then(|| {
        Rect::new(
            tab_rect.x + 1,
            tab_rect.y + 2,
            tab_rect.width.saturating_sub(2),
            tab_rect.height.saturating_sub(3),
        )
    })
}

/// Build the local proposed layouts without changing the retained snapshot.
pub(crate) fn build_placement_preview_snapshot(
    placement_snapshot: &PlacementSnapshot,
    placement_target: Option<&PanePlacementTarget>,
) -> PlacementSnapshot {
    placement_target
        .and_then(|placement_target| {
            build_proposed_placement_snapshot(placement_snapshot, placement_target)
        })
        .unwrap_or_else(|| placement_snapshot.clone())
}

fn build_proposed_placement_snapshot(
    placement_snapshot: &PlacementSnapshot,
    placement_target: &PanePlacementTarget,
) -> Option<PlacementSnapshot> {
    let layout_target = build_layout_placement_target(placement_target)?;
    if let PanePlacementTarget::Split {
        destination_tab_id, ..
    } = placement_target
    {
        if *destination_tab_id != placement_snapshot.destination_tab_id {
            return None;
        }
    }
    let source_tab_snapshot = &placement_snapshot.source_tab_snapshot;
    let source_tab_rect =
        CoreRect::from_size_at_origin(source_tab_snapshot.tab_snapshot.effective_cell_size);
    let mut pane_slot_by_id = source_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| (pane_slot.pane_id, pane_slot.clone()))
        .collect::<HashMap<_, _>>();
    let mut pane_snapshots = source_tab_snapshot.pane_snapshots.clone();

    let Some(destination_tab_snapshot) = placement_snapshot.destination_tab_snapshot.as_ref()
    else {
        let proposed_layout_tree = place_pane_within_tab(
            &source_tab_snapshot.layout_tree,
            placement_snapshot.source_pane_id,
            &layout_target,
            source_tab_rect,
            placement_snapshot.pane_sizing,
        )
        .ok()?;
        let proposed_source_tab_snapshot = build_proposed_placement_tab_snapshot(
            source_tab_snapshot,
            &proposed_layout_tree,
            &pane_slot_by_id,
            &pane_snapshots,
            placement_snapshot.pane_sizing,
        )?;
        let mut proposed_snapshot = placement_snapshot.clone();
        proposed_snapshot.source_tab_snapshot = proposed_source_tab_snapshot;
        return Some(proposed_snapshot);
    };

    for pane_slot in &destination_tab_snapshot.tab_snapshot.pane_slots {
        pane_slot_by_id
            .entry(pane_slot.pane_id)
            .or_insert_with(|| pane_slot.clone());
    }
    for pane_snapshot in &destination_tab_snapshot.pane_snapshots {
        if pane_snapshots
            .iter()
            .all(|known_pane_snapshot| known_pane_snapshot.pane_id != pane_snapshot.pane_id)
        {
            pane_snapshots.push(pane_snapshot.clone());
        }
    }
    let destination_tab_rect =
        CoreRect::from_size_at_origin(destination_tab_snapshot.tab_snapshot.effective_cell_size);
    let cross_tab_placement = place_pane_across_tabs(
        &source_tab_snapshot.layout_tree,
        placement_snapshot.source_pane_id,
        &destination_tab_snapshot.layout_tree,
        &layout_target,
        destination_tab_rect,
        placement_snapshot.pane_sizing,
    )
    .ok()?;
    let proposed_source_tab_snapshot = match cross_tab_placement.source_tree.as_ref() {
        Some(source_layout_tree) => build_proposed_placement_tab_snapshot(
            source_tab_snapshot,
            source_layout_tree,
            &pane_slot_by_id,
            &pane_snapshots,
            placement_snapshot.pane_sizing,
        )?,
        None => build_empty_placement_tab_snapshot(source_tab_snapshot),
    };
    let proposed_destination_tab_snapshot = build_proposed_placement_tab_snapshot(
        destination_tab_snapshot,
        &cross_tab_placement.destination_tree,
        &pane_slot_by_id,
        &pane_snapshots,
        placement_snapshot.pane_sizing,
    )?;
    let mut proposed_snapshot = placement_snapshot.clone();
    proposed_snapshot.source_tab_snapshot = proposed_source_tab_snapshot;
    proposed_snapshot.destination_tab_snapshot = Some(proposed_destination_tab_snapshot);
    Some(proposed_snapshot)
}

/// Interpolate the visible placement rectangles between two local snapshots.
pub(crate) fn interpolate_placement_snapshot(
    from_snapshot: &PlacementSnapshot,
    to_snapshot: &PlacementSnapshot,
    progress: f32,
) -> PlacementSnapshot {
    let progress = progress.clamp(0.0, 1.0);
    if progress == 0.0 {
        return from_snapshot.clone();
    }
    if progress == 1.0 {
        return to_snapshot.clone();
    }
    let mut animated_snapshot = to_snapshot.clone();
    animated_snapshot.source_tab_snapshot = interpolate_placement_tab_snapshot(
        &from_snapshot.source_tab_snapshot,
        &to_snapshot.source_tab_snapshot,
        progress,
    );
    animated_snapshot.destination_tab_snapshot = match (
        from_snapshot.destination_tab_snapshot.as_ref(),
        to_snapshot.destination_tab_snapshot.as_ref(),
    ) {
        (Some(from_tab_snapshot), Some(to_tab_snapshot)) => Some(
            interpolate_placement_tab_snapshot(from_tab_snapshot, to_tab_snapshot, progress),
        ),
        (Some(from_tab_snapshot), None) if progress < 1.0 => Some(from_tab_snapshot.clone()),
        (None, Some(to_tab_snapshot)) if progress > 0.0 => Some(to_tab_snapshot.clone()),
        _ => None,
    };
    animated_snapshot
}

/// Interpolate every pane slot in one placement tab.
fn interpolate_placement_tab_snapshot(
    from_tab_snapshot: &PlacementTabSnapshot,
    to_tab_snapshot: &PlacementTabSnapshot,
    progress: f32,
) -> PlacementTabSnapshot {
    let mut pane_ids = to_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect::<Vec<_>>();
    let existing_pane_ids = pane_ids.clone();
    pane_ids.extend(
        from_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .map(|pane_slot| pane_slot.pane_id)
            .filter(|pane_id| !existing_pane_ids.contains(pane_id)),
    );
    let pane_slots = pane_ids
        .into_iter()
        .filter_map(|pane_id| {
            let from_pane_slot = from_tab_snapshot
                .tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == pane_id);
            let to_pane_slot = to_tab_snapshot
                .tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == pane_id);
            let template_pane_slot = to_pane_slot.or(from_pane_slot)?;
            let mut pane_slot = template_pane_slot.clone();
            pane_slot.outer_rect = interpolate_rect(
                from_pane_slot.map(|pane_slot| pane_slot.outer_rect),
                to_pane_slot.map(|pane_slot| pane_slot.outer_rect),
                progress,
            )?;
            pane_slot.content_rect = interpolate_rect(
                from_pane_slot.and_then(|pane_slot| pane_slot.content_rect),
                to_pane_slot.and_then(|pane_slot| pane_slot.content_rect),
                progress,
            );
            pane_slot.is_visible = !pane_slot.outer_rect.is_empty()
                && (from_pane_slot.is_some_and(|pane_slot| pane_slot.is_visible)
                    || to_pane_slot.is_some_and(|pane_slot| pane_slot.is_visible));
            pane_slot.is_suppressed = if progress < 0.5 {
                from_pane_slot.map_or(pane_slot.is_suppressed, |pane_slot| pane_slot.is_suppressed)
            } else {
                to_pane_slot.map_or(pane_slot.is_suppressed, |pane_slot| pane_slot.is_suppressed)
            };
            Some(pane_slot)
        })
        .collect();
    let mut animated_tab_snapshot = to_tab_snapshot.clone();
    animated_tab_snapshot.tab_snapshot.pane_slots = pane_slots;
    animated_tab_snapshot.tab_snapshot.stack_headers = if progress < 0.5 {
        from_tab_snapshot.tab_snapshot.stack_headers.clone()
    } else {
        to_tab_snapshot.tab_snapshot.stack_headers.clone()
    };
    animated_tab_snapshot.tab_snapshot.layout_mode = if progress < 0.5 {
        from_tab_snapshot.tab_snapshot.layout_mode
    } else {
        to_tab_snapshot.tab_snapshot.layout_mode
    };
    animated_tab_snapshot.tab_snapshot.are_all_panes_suppressed = if progress < 0.5 {
        from_tab_snapshot.tab_snapshot.are_all_panes_suppressed
    } else {
        to_tab_snapshot.tab_snapshot.are_all_panes_suppressed
    };
    let mut pane_snapshots = to_tab_snapshot.pane_snapshots.clone();
    let known_pane_snapshots = pane_snapshots.clone();
    pane_snapshots.extend(
        from_tab_snapshot
            .pane_snapshots
            .iter()
            .filter(|pane_snapshot| {
                known_pane_snapshots
                    .iter()
                    .all(|known_pane_snapshot| known_pane_snapshot.pane_id != pane_snapshot.pane_id)
            })
            .cloned(),
    );
    animated_tab_snapshot.pane_snapshots = pane_snapshots;
    animated_tab_snapshot
}

/// Interpolate one pane rectangle without overshoot.
fn interpolate_rect(
    from_rect: Option<CoreRect>,
    to_rect: Option<CoreRect>,
    progress: f32,
) -> Option<CoreRect> {
    match (from_rect, to_rect) {
        (Some(from_rect), Some(to_rect)) => Some(CoreRect::from_origin_and_size(
            interpolate_point(from_rect.origin, to_rect.origin, progress),
            interpolate_size(from_rect.cell_size, to_rect.cell_size, progress),
        )),
        (Some(from_rect), None) if progress < 1.0 => Some(CoreRect::from_origin_and_size(
            from_rect.origin,
            interpolate_size(
                from_rect.cell_size,
                Size {
                    column_count: 0,
                    row_count: 0,
                },
                progress,
            ),
        )),
        (None, Some(to_rect)) if progress > 0.0 => Some(CoreRect::from_origin_and_size(
            to_rect.origin,
            interpolate_size(
                Size {
                    column_count: 0,
                    row_count: 0,
                },
                to_rect.cell_size,
                progress,
            ),
        )),
        _ => None,
    }
}

fn interpolate_point(from_point: Point, to_point: Point, progress: f32) -> Point {
    Point {
        column: interpolate_coordinate(from_point.column, to_point.column, progress),
        row: interpolate_coordinate(from_point.row, to_point.row, progress),
    }
}

fn interpolate_size(from_size: Size, to_size: Size, progress: f32) -> Size {
    Size {
        column_count: interpolate_coordinate(
            from_size.column_count,
            to_size.column_count,
            progress,
        ),
        row_count: interpolate_coordinate(from_size.row_count, to_size.row_count, progress),
    }
}

fn interpolate_coordinate(from_coordinate: u16, to_coordinate: u16, progress: f32) -> u16 {
    let from_coordinate = f32::from(from_coordinate);
    let to_coordinate = f32::from(to_coordinate);
    (from_coordinate + (to_coordinate - from_coordinate) * progress)
        .round()
        .clamp(0.0, f32::from(u16::MAX)) as u16
}

/// Convert the viewer target into the pure layout target used for the draft.
pub(crate) fn build_layout_placement_target(
    placement_target: &PanePlacementTarget,
) -> Option<PlacementTarget> {
    match placement_target {
        PanePlacementTarget::Swap { target_pane_id } => Some(PlacementTarget::Swap {
            target_pane_id: *target_pane_id,
        }),
        PanePlacementTarget::Split {
            anchor, direction, ..
        } => Some(PlacementTarget::Insert {
            anchor: anchor.clone(),
            direction: *direction,
        }),
    }
}

/// Build a solved tab snapshot for one proposed layout tree.
fn build_proposed_placement_tab_snapshot(
    base_tab_snapshot: &PlacementTabSnapshot,
    layout_tree: &koshi_layout::tree::LayoutNode,
    pane_slot_by_id: &HashMap<PaneId, PaneSlot>,
    pane_snapshots: &[PlacementPaneSnapshot],
    pane_sizing: koshi_layout::solver::PaneSizing,
) -> Option<PlacementTabSnapshot> {
    let layout_solve = solve_layout_with_mode(
        layout_tree,
        LayoutMode::Tiled,
        CoreRect::from_size_at_origin(base_tab_snapshot.tab_snapshot.effective_cell_size),
        pane_sizing,
    );
    let content_rect_by_pane_id = list_content_rects(&layout_solve)
        .into_iter()
        .collect::<HashMap<_, _>>();
    let suppressed_pane_ids = layout_solve
        .suppressed_pane_ids
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let pane_slots = layout_solve
        .pane_rects
        .iter()
        .map(|(pane_id, outer_rect)| {
            let template_pane_slot = pane_slot_by_id.get(pane_id)?;
            let content_rect = content_rect_by_pane_id.get(pane_id).copied().flatten();
            Some(PaneSlot {
                pane_id: *pane_id,
                outer_rect: *outer_rect,
                content_rect,
                pane_kind: template_pane_slot.pane_kind,
                is_visible: content_rect.is_some(),
                is_suppressed: suppressed_pane_ids.contains(pane_id),
                is_dead: template_pane_slot.is_dead,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let proposed_pane_snapshots = pane_slots
        .iter()
        .map(|pane_slot| {
            pane_snapshots
                .iter()
                .find(|pane_snapshot| pane_snapshot.pane_id == pane_slot.pane_id)
                .cloned()
        })
        .collect::<Option<Vec<_>>>()?;
    let mut proposed_tab_snapshot = base_tab_snapshot.clone();
    proposed_tab_snapshot.layout_tree = layout_tree.clone();
    proposed_tab_snapshot.tab_snapshot.pane_slots = pane_slots;
    proposed_tab_snapshot.tab_snapshot.stack_headers = layout_solve.stack_headers;
    proposed_tab_snapshot.tab_snapshot.layout_mode = LayoutMode::Tiled;
    proposed_tab_snapshot.tab_snapshot.are_all_panes_suppressed =
        layout_solve.is_all_panes_suppressed;
    proposed_tab_snapshot.pane_snapshots = proposed_pane_snapshots;
    Some(proposed_tab_snapshot)
}

/// Build the source view after its only pane leaves a different tab.
fn build_empty_placement_tab_snapshot(
    base_tab_snapshot: &PlacementTabSnapshot,
) -> PlacementTabSnapshot {
    let mut empty_tab_snapshot = base_tab_snapshot.clone();
    empty_tab_snapshot.tab_snapshot.pane_slots.clear();
    empty_tab_snapshot.tab_snapshot.stack_headers.clear();
    empty_tab_snapshot.tab_snapshot.are_all_panes_suppressed = false;
    empty_tab_snapshot.tab_snapshot.layout_mode = LayoutMode::Tiled;
    empty_tab_snapshot.pane_snapshots.clear();
    empty_tab_snapshot
}

/// Build native image paints for the two placement-preview panels.
fn build_placement_image_paints(
    placement_display_snapshot: &PlacementSnapshot,
    render_area: Rect,
) -> Vec<ImagePaint> {
    let Some((source_rect, destination_rect)) = compute_placement_preview_tab_rects(render_area)
    else {
        return Vec::new();
    };
    let destination_snapshot = placement_display_snapshot
        .destination_tab_snapshot
        .as_ref()
        .unwrap_or(&placement_display_snapshot.source_tab_snapshot);
    let mut image_paints = Vec::new();
    append_placement_tab_image_paints(
        &placement_display_snapshot.source_tab_snapshot,
        source_rect,
        0,
        render_area,
        &mut image_paints,
    );
    append_placement_tab_image_paints(
        destination_snapshot,
        destination_rect,
        1,
        render_area,
        &mut image_paints,
    );
    image_paints.sort_by_key(|image_paint| {
        (
            image_paint.z_index,
            image_paint.image_record.display.image_id.unwrap_or(0),
            image_paint.image_record.display.placement_id.unwrap_or(0),
            image_paint.pane_id,
            image_paint.placement_id,
        )
    });
    image_paints
}

/// Append native image paints for one proposed preview panel.
fn append_placement_tab_image_paints(
    tab_snapshot: &PlacementTabSnapshot,
    tab_rect: Rect,
    panel_index: u8,
    render_area: Rect,
    image_paints: &mut Vec<ImagePaint>,
) {
    let Some(layout_rect) = compute_placement_tab_layout_rect(tab_rect) else {
        return;
    };
    let layout_size = tab_snapshot.tab_snapshot.effective_cell_size;
    if layout_size.column_count == 0 || layout_size.row_count == 0 {
        return;
    }
    for pane_slot in &tab_snapshot.tab_snapshot.pane_slots {
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(mini_content_rect) = project_core_rect(content_rect, layout_size, layout_rect)
        else {
            continue;
        };
        let Some(pane_snapshot) = tab_snapshot
            .pane_snapshots
            .iter()
            .find(|pane_snapshot| pane_snapshot.pane_id == pane_slot.pane_id)
        else {
            continue;
        };
        if pane_snapshot.terminal_grid_view.is_none() {
            continue;
        }
        for image_placement_snapshot in &pane_snapshot.image_placement_snapshots {
            let Some(image_record) = image_placement_snapshot.clone_image_record() else {
                continue;
            };
            let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
            let (anchor_row, anchor_column) = image_placement_snapshot.get_anchor_cell();
            let image_rect = CoreRect::from_origin_and_size(
                Point {
                    column: content_rect.origin.column.saturating_add(anchor_column),
                    row: content_rect.origin.row.saturating_add(anchor_row),
                },
                Size {
                    column_count,
                    row_count,
                },
            );
            let Some(visible_image_rect) = image_rect.compute_intersection(content_rect) else {
                continue;
            };
            let Some(mini_image_rect) =
                project_core_rect(visible_image_rect, layout_size, layout_rect)
            else {
                continue;
            };
            let target_area = mini_image_rect
                .intersection(mini_content_rect)
                .intersection(render_area);
            if target_area.width == 0 || target_area.height == 0 {
                continue;
            }
            let Some(source_rect) = build_preview_image_source_rect(
                mini_image_rect,
                target_area,
                visible_image_rect.cell_size,
                image_placement_snapshot,
                &image_record,
            ) else {
                continue;
            };
            if source_rect.pixel_width == 0 || source_rect.pixel_height == 0 {
                continue;
            }
            let mut image_paint = ImagePaint::from_image_placement(
                pane_snapshot.pane_id,
                image_placement_snapshot.get_placement_id(),
                image_record,
                target_area,
                source_rect,
                image_placement_snapshot
                    .get_image_record()
                    .map_or(0, |image_record| image_record.display.z_index),
            )
            .with_placement_preview_key(panel_index);
            image_paint.image_content_id = image_placement_snapshot.get_image_content_id();
            image_paint.cell_pixel_offset_x = None;
            image_paint.cell_pixel_offset_y = None;
            image_paints.push(image_paint);
        }
    }
}

/// Map a projected preview rectangle to its proportional source pixels.
fn build_preview_image_source_rect(
    image_rect: Rect,
    target_area: Rect,
    visible_cell_size: Size,
    image_placement_snapshot: &ImagePlacementSnapshot,
    image_record: &ImageRecord,
) -> Option<ImageSourceRect> {
    let (source_origin_x, source_origin_y, source_pixel_width, source_pixel_height) =
        image_record.compute_source_rect().ok()?;
    let cell_geometry = image_placement_snapshot.get_cell_geometry();
    let source_x = compute_preview_source_span(
        target_area.x.checked_sub(image_rect.x)?,
        target_area.right().checked_sub(image_rect.x)?,
        image_rect.width,
        cell_geometry.cell_offset.column,
        visible_cell_size.column_count,
        cell_geometry.full_size.column_count,
        source_pixel_width,
    )?;
    let source_y = compute_preview_source_span(
        target_area.y.checked_sub(image_rect.y)?,
        target_area.bottom().checked_sub(image_rect.y)?,
        image_rect.height,
        cell_geometry.cell_offset.row,
        visible_cell_size.row_count,
        cell_geometry.full_size.row_count,
        source_pixel_height,
    )?;
    Some(ImageSourceRect {
        pixel_x: source_origin_x.checked_add(source_x.0)?,
        pixel_y: source_origin_y.checked_add(source_y.0)?,
        pixel_width: source_x.1,
        pixel_height: source_y.1,
    })
}

/// Map a projected preview span to a source-pixel span.
fn compute_preview_source_span(
    target_start: u16,
    target_end: u16,
    target_length: u16,
    cell_offset: u16,
    visible_cell_length: u16,
    full_cell_length: u16,
    source_pixel_length: u32,
) -> Option<(u32, u32)> {
    if target_start >= target_end
        || target_end > target_length
        || target_length == 0
        || visible_cell_length == 0
        || full_cell_length == 0
        || u32::from(cell_offset).saturating_add(u32::from(visible_cell_length))
            > u32::from(full_cell_length)
        || source_pixel_length == 0
    {
        return None;
    }
    let target_length = u64::from(target_length);
    let cell_start = u64::from(cell_offset)
        + u64::from(target_start) * u64::from(visible_cell_length) / target_length;
    let cell_end = u64::from(cell_offset)
        + (u64::from(target_end) * u64::from(visible_cell_length)).div_ceil(target_length);
    let full_cell_length = u64::from(full_cell_length);
    let pixel_start = (cell_start * u64::from(source_pixel_length)) / full_cell_length;
    let pixel_end = (cell_end * u64::from(source_pixel_length)).div_ceil(full_cell_length);
    let pixel_start = u32::try_from(pixel_start.min(u64::from(source_pixel_length))).ok()?;
    let pixel_end = u32::try_from(pixel_end.min(u64::from(source_pixel_length))).ok()?;
    (pixel_end > pixel_start).then_some((pixel_start, pixel_end - pixel_start))
}

/// Draw one proposed tab layout, including bounded terminal cells and images.
#[allow(clippy::too_many_arguments)]
fn draw_placement_tab(
    tab_snapshot: &PlacementTabSnapshot,
    base_tab_snapshot: &PlacementTabSnapshot,
    tab_rect: Rect,
    label: &str,
    source_pane_id: PaneId,
    placement_target: Option<&PanePlacementTarget>,
    panel_index: u8,
    theme: &Theme,
    image_mode: ImageRenderMode,
    prepared_preview_image_keys: Option<&[PlacementPreviewImageKey]>,
    render_buffer: &mut Buffer,
) {
    let tab_style = Style::default()
        .fg(theme.dimmed_ramp_text_color)
        .bg(theme.bar_background_color);
    for row_index in tab_rect.y..tab_rect.y + tab_rect.height {
        render_buffer.set_string(
            tab_rect.x,
            row_index,
            " ".repeat(usize::from(tab_rect.width)),
            tab_style,
        );
    }
    let tab_title = format!(" {label}: {} ", tab_snapshot.tab_snapshot.tab_name);
    render_buffer.set_string(tab_rect.x, tab_rect.y, tab_title, tab_style);
    if tab_rect.width < 8 || tab_rect.height < 5 {
        return;
    }
    let Some(layout_rect) = compute_placement_tab_layout_rect(tab_rect) else {
        return;
    };
    let layout_size = tab_snapshot.tab_snapshot.effective_cell_size;
    if layout_size.column_count == 0 || layout_size.row_count == 0 {
        render_buffer.set_string(layout_rect.x, layout_rect.y, "no visible panes", tab_style);
        return;
    }

    for pane_slot in &tab_snapshot.tab_snapshot.pane_slots {
        let Some(mini_outer_rect) =
            project_core_rect(pane_slot.outer_rect, layout_size, layout_rect)
        else {
            continue;
        };
        if mini_outer_rect.width < 2 || mini_outer_rect.height < 2 {
            continue;
        }
        let pane_style = if pane_slot.pane_id == source_pane_id {
            Style::default()
                .fg(theme.accent_color)
                .bg(theme.bar_background_color)
        } else if is_target_pane(pane_slot.pane_id, placement_target) {
            Style::default()
                .fg(theme.focused_border_color)
                .bg(theme.bar_background_color)
        } else {
            Style::default()
                .fg(theme.unfocused_border_color)
                .bg(theme.bar_background_color)
        };
        if let Some(content_rect) = pane_slot.content_rect {
            if let Some(mini_content_rect) =
                project_core_rect(content_rect, layout_size, layout_rect)
            {
                if let Some(pane_snapshot) = tab_snapshot
                    .pane_snapshots
                    .iter()
                    .find(|pane_snapshot| pane_snapshot.pane_id == pane_slot.pane_id)
                {
                    if let Some(grid_view) = pane_snapshot.terminal_grid_view.as_ref() {
                        draw_grid_preview(&grid_view.grid, mini_content_rect, render_buffer);
                    }
                    draw_placement_image_markers(
                        pane_snapshot,
                        content_rect,
                        layout_size,
                        layout_rect,
                        panel_index,
                        image_mode,
                        prepared_preview_image_keys,
                        render_buffer,
                    );
                }
            }
        }
        draw_box_border(mini_outer_rect, pane_style, render_buffer);
        let pane_label = if pane_slot.pane_id == source_pane_id {
            " SRC ".to_owned()
        } else {
            let pane_id_text = pane_slot.pane_id.to_string();
            format!(" {} ", pane_id_text.chars().take(4).collect::<String>())
        };
        render_buffer.set_string(
            mini_outer_rect.x + 1,
            mini_outer_rect.y,
            pane_label,
            pane_style,
        );
    }

    for stack_header in &tab_snapshot.tab_snapshot.stack_headers {
        let Some(mini_header_rect) =
            project_core_rect(stack_header.header_rect, layout_size, layout_rect)
        else {
            continue;
        };
        render_buffer.set_string(
            mini_header_rect.x,
            mini_header_rect.y,
            format!(" H{} ", stack_header.member_index + 1),
            Style::default()
                .fg(theme.stack_header_text_color)
                .bg(theme.stack_header_background_color),
        );
    }

    if let Some(target_outline) = build_target_outline_rect(
        base_tab_snapshot,
        placement_target,
        layout_size,
        layout_rect,
    ) {
        draw_box_border(
            target_outline,
            Style::default()
                .fg(theme.accent_color)
                .bg(theme.bar_background_color),
            render_buffer,
        );
    }
}

/// Draw unavailable-image text over images the outer terminal cannot paint.
#[allow(clippy::too_many_arguments)]
fn draw_placement_image_markers(
    pane_snapshot: &PlacementPaneSnapshot,
    content_rect: CoreRect,
    layout_size: Size,
    layout_rect: Rect,
    panel_index: u8,
    image_mode: ImageRenderMode,
    prepared_preview_image_keys: Option<&[PlacementPreviewImageKey]>,
    render_buffer: &mut Buffer,
) {
    let mut placeholder_rects = Vec::new();
    for image_placement_snapshot in &pane_snapshot.image_placement_snapshots {
        let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
        let (anchor_row, anchor_column) = image_placement_snapshot.get_anchor_cell();
        let image_rect = CoreRect::from_origin_and_size(
            Point {
                column: content_rect.origin.column.saturating_add(anchor_column),
                row: content_rect.origin.row.saturating_add(anchor_row),
            },
            Size {
                column_count,
                row_count,
            },
        );
        let Some(visible_image_rect) = image_rect.compute_intersection(content_rect) else {
            continue;
        };
        let Some(mini_image_rect) = project_core_rect(visible_image_rect, layout_size, layout_rect)
        else {
            continue;
        };
        let is_native_image_ready = image_mode == ImageRenderMode::Native
            && image_placement_snapshot.get_image_record().is_some()
            && prepared_preview_image_keys.is_some_and(|image_keys| {
                image_keys.contains(&PlacementPreviewImageKey {
                    panel_index,
                    pane_id: pane_snapshot.pane_id,
                    placement_id: image_placement_snapshot.get_placement_id(),
                })
            });
        if !is_native_image_ready {
            placeholder_rects.push(mini_image_rect);
        }
    }
    draw_image_placeholders(&placeholder_rects, render_buffer);
}

/// Project a core cell rectangle into one ratatui preview rectangle.
pub(crate) fn project_core_rect(
    core_rect: CoreRect,
    source_size: Size,
    target_rect: Rect,
) -> Option<Rect> {
    if core_rect.is_empty()
        || source_size.column_count == 0
        || source_size.row_count == 0
        || target_rect.width == 0
        || target_rect.height == 0
    {
        return None;
    }
    let (column, column_count) = project_axis(
        core_rect.origin.column,
        core_rect.cell_size.column_count,
        source_size.column_count,
        target_rect.x,
        target_rect.width,
    );
    let (row, row_count) = project_axis(
        core_rect.origin.row,
        core_rect.cell_size.row_count,
        source_size.row_count,
        target_rect.y,
        target_rect.height,
    );
    (column_count > 0 && row_count > 0).then_some(Rect::new(column, row, column_count, row_count))
}

/// Project one core axis without overflowing a ratatui coordinate.
fn project_axis(
    source_axis_origin: u16,
    source_axis_length: u16,
    source_axis_total_length: u16,
    target_axis_origin: u16,
    target_axis_total_length: u16,
) -> (u16, u16) {
    let source_axis_start = u32::from(source_axis_origin);
    let source_axis_end = source_axis_start.saturating_add(u32::from(source_axis_length));
    let source_axis_total_length = u32::from(source_axis_total_length);
    let target_axis_total_length = u32::from(target_axis_total_length);
    let projected_start_offset = source_axis_start
        .saturating_mul(target_axis_total_length)
        .checked_div(source_axis_total_length)
        .unwrap_or(0)
        .min(target_axis_total_length);
    let projected_end_offset = source_axis_end
        .saturating_mul(target_axis_total_length)
        .checked_div(source_axis_total_length)
        .unwrap_or(0)
        .min(target_axis_total_length);
    let projected_axis_origin = u32::from(target_axis_origin)
        .saturating_add(projected_start_offset)
        .min(u32::from(u16::MAX)) as u16;
    let projected_axis_end = u32::from(target_axis_origin)
        .saturating_add(projected_end_offset)
        .min(u32::from(u16::MAX));
    let projected_axis_length = projected_axis_end
        .saturating_sub(u32::from(projected_axis_origin))
        .min(u32::from(u16::MAX)) as u16;
    (projected_axis_origin, projected_axis_length)
}

/// Return whether a pane belongs to the selected destination span.
fn is_target_pane(pane_id: PaneId, placement_target: Option<&PanePlacementTarget>) -> bool {
    let Some(placement_target) = placement_target else {
        return false;
    };
    match placement_target {
        PanePlacementTarget::Swap { target_pane_id } => pane_id == *target_pane_id,
        PanePlacementTarget::Split { anchor, .. } => match anchor {
            PanePlacementAnchor::Pane(target_pane_id) => pane_id == *target_pane_id,
            PanePlacementAnchor::Group(target_pane_ids) => target_pane_ids.contains(&pane_id),
            PanePlacementAnchor::Tab => true,
        },
    }
}

/// Return the fixed outline for the selected base destination span.
fn build_target_outline_rect(
    base_tab_snapshot: &PlacementTabSnapshot,
    placement_target: Option<&PanePlacementTarget>,
    layout_size: Size,
    layout_rect: Rect,
) -> Option<Rect> {
    let placement_target = placement_target?;
    if matches!(
        placement_target,
        PanePlacementTarget::Split {
            anchor: PanePlacementAnchor::Tab,
            ..
        }
    ) {
        return Some(layout_rect);
    }
    let mut target_outline: Option<Rect> = None;
    for pane_slot in &base_tab_snapshot.tab_snapshot.pane_slots {
        if !is_target_pane(pane_slot.pane_id, Some(placement_target)) {
            continue;
        }
        let mini_outer_rect = project_core_rect(pane_slot.outer_rect, layout_size, layout_rect)?;
        target_outline = Some(match target_outline {
            Some(current_outline) => compute_union_rect(current_outline, mini_outer_rect),
            None => mini_outer_rect,
        });
    }
    target_outline
}

/// Return the smallest ratatui rectangle containing both rectangles.
fn compute_union_rect(first_rect: Rect, second_rect: Rect) -> Rect {
    let left = first_rect.x.min(second_rect.x);
    let top = first_rect.y.min(second_rect.y);
    let right = u32::from(first_rect.right()).max(u32::from(second_rect.right()));
    let bottom = u32::from(first_rect.bottom()).max(u32::from(second_rect.bottom()));
    Rect::new(
        left,
        top,
        right
            .saturating_sub(u32::from(left))
            .min(u32::from(u16::MAX)) as u16,
        bottom
            .saturating_sub(u32::from(top))
            .min(u32::from(u16::MAX)) as u16,
    )
}

/// Draw one border around the placement overlay.
fn draw_box_border(panel_rect: Rect, style: Style, render_buffer: &mut Buffer) {
    if panel_rect.width < 2 || panel_rect.height < 2 {
        return;
    }
    let horizontal_border = "─".repeat(usize::from(panel_rect.width.saturating_sub(2)));
    render_buffer.set_string(panel_rect.x + 1, panel_rect.y, &horizontal_border, style);
    render_buffer.set_string(
        panel_rect.x + 1,
        panel_rect.y + panel_rect.height - 1,
        &horizontal_border,
        style,
    );
    for row_index in panel_rect.y + 1..panel_rect.y + panel_rect.height - 1 {
        render_buffer.set_string(panel_rect.x, row_index, "│", style);
        render_buffer.set_string(panel_rect.x + panel_rect.width - 1, row_index, "│", style);
    }
    render_buffer.set_string(panel_rect.x, panel_rect.y, "┌", style);
    render_buffer.set_string(
        panel_rect.x + panel_rect.width - 1,
        panel_rect.y,
        "┐",
        style,
    );
    render_buffer.set_string(
        panel_rect.x,
        panel_rect.y + panel_rect.height - 1,
        "└",
        style,
    );
    render_buffer.set_string(
        panel_rect.x + panel_rect.width - 1,
        panel_rect.y + panel_rect.height - 1,
        "┘",
        style,
    );
}

/// The graphics capability proved by a reply from the outer terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphicsSupport {
    /// Paint image coverage with the fixed unsupported-image text.
    Unsupported,
    /// Emit Kitty raw-RGBA image commands after the text buffer is painted.
    Kitty,
    /// Advertise iTerm2 images for connection-local worker output.
    Iterm,
    /// Advertise Sixel images for connection-local worker output.
    Sixel {
        /// Number of colors available to the Sixel encoder.
        palette_color_count: usize,
        /// Maximum Sixel width in pixels, or `None` when the host reports no limit.
        max_pixel_width: Option<u32>,
        /// Maximum Sixel height in pixels, or `None` when the host reports no limit.
        max_pixel_height: Option<u32>,
    },
}

/// A failure while painting a frame or emitting its native image data.
#[derive(Debug)]
pub(crate) enum PaintError<BackendError> {
    /// The ratatui backend did not accept the frame buffer.
    Backend(BackendError),
    /// The native image writer did not accept its output.
    Image(io::Error),
}

impl GraphicsSupport {
    /// Map the terminal capability to the renderer's image mode.
    pub(crate) fn get_image_render_mode(self) -> ImageRenderMode {
        match self {
            Self::Unsupported => ImageRenderMode::Placeholder,
            Self::Kitty => ImageRenderMode::Native,
            Self::Iterm | Self::Sixel { .. } => ImageRenderMode::Native,
        }
    }

    /// Convert the selected connection-local protocol to its attach report.
    pub(crate) const fn build_graphics_capabilities(self) -> GraphicsCapabilities {
        match self {
            Self::Unsupported => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: false,
                supports_sixel: false,
            },
            Self::Kitty => GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
            Self::Iterm => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: true,
                supports_sixel: false,
            },
            Self::Sixel { .. } => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: false,
                supports_sixel: true,
            },
        }
    }
}

/// The values gathered by one bounded terminal capability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalProbe {
    /// The preferred protocol proved by the terminal.
    graphics_support: GraphicsSupport,
    /// The cell dimensions reported by the terminal, if valid.
    cell_size: Option<PixelCellSize>,
}

impl TerminalProbe {
    /// Build the result for a terminal that answered no capability query.
    const fn unsupported() -> Self {
        Self {
            graphics_support: GraphicsSupport::Unsupported,
            cell_size: None,
        }
    }
}

/// Select the cell dimensions that a native-image terminal may report.
fn resolve_initial_cell_size(
    graphics_support: GraphicsSupport,
    probed_cell_size: Option<PixelCellSize>,
    locally_measured_cell_size: Option<PixelCellSize>,
) -> Option<PixelCellSize> {
    if matches!(graphics_support, GraphicsSupport::Unsupported) {
        None
    } else {
        probed_cell_size.or(locally_measured_cell_size)
    }
}

/// Coordinates cell-size requests with resize reports on one terminal.
#[derive(Debug)]
pub(crate) struct CellSizeQuery {
    /// The most recent measurement accepted for the current viewport.
    current_cell_size: Option<PixelCellSize>,
    /// Whether a CSI 16t request has not received its reply.
    is_reply_pending: bool,
    /// Whether the pending reply belongs to an older viewport.
    should_discard_pending_reply: bool,
    /// Whether this attachment can write terminal queries.
    is_query_enabled: bool,
}

impl CellSizeQuery {
    /// Build a coordinator with the measurement captured before Attach and
    /// whether a cell-size query is already outstanding.
    pub(crate) fn from_current_measurement(
        current_cell_size: Option<PixelCellSize>,
        is_query_enabled: bool,
        is_reply_pending: bool,
    ) -> Self {
        Self {
            current_cell_size: is_query_enabled.then_some(current_cell_size).flatten(),
            is_reply_pending: is_query_enabled && is_reply_pending,
            should_discard_pending_reply: false,
            is_query_enabled,
        }
    }

    /// Return the measurement to place in the next Attach.
    pub(crate) fn get_current_cell_size(&self) -> Option<PixelCellSize> {
        self.current_cell_size
    }

    /// Invalidate or replace the measurement for a resized viewport.
    ///
    /// A pending reply is discarded because CSI 16t carries no request id. A
    /// fresh request is needed only when the resize has no usable local metric.
    pub(crate) fn update_cell_size_for_resize(
        &mut self,
        measured_cell_size: Option<PixelCellSize>,
    ) -> bool {
        let resized_cell_size = self
            .is_query_enabled
            .then_some(measured_cell_size)
            .flatten();
        self.current_cell_size = resized_cell_size;
        if self.is_reply_pending {
            self.should_discard_pending_reply = true;
            false
        } else {
            resized_cell_size.is_none()
        }
    }

    /// Accept a reply, returning the value to send to the session and whether
    /// a fresh query must be written after discarding an older reply.
    pub(crate) fn accept_cell_size_reply(
        &mut self,
        reported_cell_size: PixelCellSize,
    ) -> (Option<PixelCellSize>, bool) {
        if !self.is_reply_pending {
            return (None, false);
        }
        self.is_reply_pending = false;
        if self.should_discard_pending_reply {
            self.should_discard_pending_reply = false;
            return (None, self.current_cell_size.is_none());
        }
        self.current_cell_size = Some(reported_cell_size);
        (Some(reported_cell_size), false)
    }

    /// Write one CSI 16t request at the attachment loop's output boundary.
    pub(crate) fn request_cell_size(&mut self) {
        if !self.is_query_enabled || self.is_reply_pending {
            return;
        }
        let mut writer = io::stdout().lock();
        if let Err(request_error) = self.write_cell_size_request(&mut writer) {
            tracing::warn!(%request_error, "could not request terminal cell dimensions");
        }
    }

    /// Write one request through `writer`, retaining pending state after an
    /// attempted write until a matching ordered reply is consumed.
    fn write_cell_size_request<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if !self.is_query_enabled || self.is_reply_pending || self.current_cell_size.is_some() {
            return Ok(());
        }
        self.is_reply_pending = true;
        writer.write_all(CELL_SIZE_QUERY_BYTES)?;
        writer.flush()
    }
}

/// The input, mode, and capability-query owner for one attached terminal.
pub(crate) struct TerminalOwner {
    /// Native graphics support proved by the terminal's protocol answer.
    graphics_support: GraphicsSupport,
    /// Initial pixel dimensions of one terminal cell from the probe or window metrics.
    initial_cell_size: Option<PixelCellSize>,
    /// Whether standard output is the terminal receiving rendered frames.
    output_is_terminal: bool,
    /// Terminal handle used for protocol output and platform-mode restoration.
    terminal: Arc<Mutex<Option<TerminalDevice>>>,
    /// Parsed input source shared with the input thread. `None` when both
    /// standard streams are redirected and no terminal work is needed.
    reader: Option<InputReader>,
    /// Wakes the input reader when this attachment ends. It is present exactly
    /// when `reader` is present.
    waker: Option<PlatformWaker>,
    /// Stops input delivery before terminal restoration.
    shutdown: Arc<AtomicBool>,
    /// Whether this attachment enabled application-level terminal modes.
    application_modes_active: Arc<AtomicBool>,
    /// Ensures one path cancels Kitty transfers and deletes this attachment's images.
    image_cleanup_claimed: Arc<AtomicBool>,
    /// The input thread started after Attach succeeds.
    input_thread: Option<thread::JoinHandle<()>>,
    /// Rejects a second activation of the same terminal.
    is_activated: bool,
}

impl TerminalOwner {
    /// Open the controlling terminal and probe its capabilities when image
    /// support is enabled. A native-image terminal whose probe did not receive
    /// a cell size uses the window's pixel dimensions through [`read_local_cell_size`].
    /// With piped input and output, build an unsupported owner without opening
    /// `/dev/tty`; that client reads no keys and writes its frame to the pipe.
    pub(crate) fn open_terminal_owner(supports_native_images: bool) -> Result<Self, String> {
        let input_is_terminal = io::stdin().is_terminal();
        let output_is_terminal = io::stdout().is_terminal();
        let (graphics_support, initial_cell_size, terminal, reader, waker) =
            if needs_terminal_device(input_is_terminal, output_is_terminal) {
                let (mut terminal, event_source) = TerminalDevice::open_terminal_device()
                    .map_err(|open_error| format!("could not open the terminal: {open_error}"))?;
                let mut reader = InputReader::from_event_source(event_source);
                let terminal_probe = if supports_native_images {
                    resolve_graphics_support_for_output(output_is_terminal, || {
                        run_with_raw_mode(
                            &mut terminal,
                            |terminal| terminal.enter_raw_mode(),
                            |terminal| probe_terminal(terminal, &mut reader),
                            |terminal| terminal.enter_cooked_mode(),
                        )
                    })
                    .map_err(|probe_error| {
                        format!("could not probe terminal graphics support: {probe_error}")
                    })?
                } else {
                    TerminalProbe::unsupported()
                };
                let locally_measured_cell_size = if terminal_probe.cell_size.is_some()
                    || matches!(
                        terminal_probe.graphics_support,
                        GraphicsSupport::Unsupported
                    ) {
                    None
                } else {
                    read_local_cell_size()
                };
                let initial_cell_size = resolve_initial_cell_size(
                    terminal_probe.graphics_support,
                    terminal_probe.cell_size,
                    locally_measured_cell_size,
                );
                let waker = reader.create_waker();
                (
                    terminal_probe.graphics_support,
                    initial_cell_size,
                    Some(terminal),
                    Some(reader),
                    Some(waker),
                )
            } else {
                (GraphicsSupport::Unsupported, None, None, None, None)
            };
        let image_cleanup_claimed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            graphics_support,
            initial_cell_size,
            output_is_terminal,
            terminal: Arc::new(Mutex::new(terminal)),
            reader,
            waker,
            shutdown: Arc::new(AtomicBool::new(false)),
            application_modes_active: Arc::new(AtomicBool::new(false)),
            image_cleanup_claimed,
            input_thread: None,
            is_activated: false,
        })
    }

    /// Return the native graphics support proved by the terminal probe.
    pub(crate) fn get_graphics_support(&self) -> GraphicsSupport {
        self.graphics_support
    }

    /// Return the cell dimensions captured during the initial terminal probe.
    #[allow(dead_code)]
    pub(crate) fn get_initial_cell_size(&self) -> Option<PixelCellSize> {
        self.initial_cell_size
    }

    /// Build the cell-size coordinator used after this terminal attaches.
    pub(crate) fn build_cell_size_query(&self) -> CellSizeQuery {
        CellSizeQuery::from_current_measurement(
            self.initial_cell_size,
            self.output_is_terminal
                && !matches!(self.graphics_support, GraphicsSupport::Unsupported),
            false,
        )
    }

    /// Register panic-safe image cleanup and terminal restoration.
    pub(crate) fn register_restore(&self, cleanup: &TerminalCleanupGuard) {
        let terminal = Arc::clone(&self.terminal);
        let graphics_support = self.graphics_support;
        let shutdown = Arc::clone(&self.shutdown);
        let waker = self.waker.clone();
        let application_modes_active = Arc::clone(&self.application_modes_active);
        let image_cleanup_claimed = Arc::clone(&self.image_cleanup_claimed);
        cleanup.register_cleanup(Box::new(move || {
            shutdown.store(true, Ordering::Release);
            if let Some(waker) = &waker {
                let _ = waker.wake();
            }
            try_restore_shared_terminal(
                &terminal,
                graphics_support,
                &application_modes_active,
                &image_cleanup_claimed,
            );
        }));
    }

    /// Enable terminal modes and start input delivery after Attach succeeds.
    pub(crate) fn activate(
        &mut self,
        runtime_event_sender: mpsc::SyncSender<RuntimeEvent>,
        client_id: ClientId,
        should_read_input: bool,
    ) -> Result<(), String> {
        if self.is_activated {
            return Err("terminal owner was already activated".to_string());
        }
        self.is_activated = true;
        {
            let mut terminal_guard = lock_terminal(&self.terminal);
            if let Some(terminal) = terminal_guard.as_mut() {
                terminal.enter_raw_mode().map_err(|raw_mode_error| {
                    format!("could not enter terminal raw mode: {raw_mode_error}")
                })?;
                if self.output_is_terminal {
                    self.application_modes_active.store(true, Ordering::Release);
                    enable_terminal_modes(terminal, self.graphics_support).map_err(
                        |mode_enable_error| {
                            format!("could not enable terminal modes: {mode_enable_error}")
                        },
                    )?;
                }
            } else if self.output_is_terminal || should_read_input {
                return Err("terminal owner was already restored".to_string());
            }
        }
        if !should_read_input {
            return Ok(());
        }

        let mut input_reader = self
            .reader
            .take()
            .ok_or_else(|| "terminal input reader is unavailable".to_string())?;
        let shutdown = Arc::clone(&self.shutdown);
        let panic_event_sender = runtime_event_sender.clone();
        self.input_thread = Some(
            thread::Builder::new()
                .name("koshi-terminal-input".to_string())
                .spawn(move || {
                    let input_thread_result = catch_unwind(AssertUnwindSafe(|| {
                        run_terminal_input(
                            &mut input_reader,
                            &runtime_event_sender,
                            client_id,
                            &shutdown,
                        );
                    }));
                    if input_thread_result.is_err() {
                        let _ = panic_event_sender.send(RuntimeEvent::Quit);
                    }
                })
                .map_err(|thread_spawn_error| {
                    format!("could not spawn the terminal input thread: {thread_spawn_error}")
                })?,
        );
        Ok(())
    }

    /// Stop input delivery and restore the host terminal state.
    pub(crate) fn shutdown(mut self) {
        self.stop_terminal_owner();
    }

    /// Signal and join the input thread, then restore every terminal mode.
    fn stop_terminal_owner(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(waker) = &self.waker {
            let _ = waker.wake();
        }
        if let Some(input_thread_handle) = self.input_thread.take() {
            let _ = input_thread_handle.join();
        }
        restore_shared_terminal(
            &self.terminal,
            self.graphics_support,
            &self.application_modes_active,
            &self.image_cleanup_claimed,
        );
    }
}

impl Drop for TerminalOwner {
    fn drop(&mut self) {
        self.stop_terminal_owner();
    }
}

/// Whether input or output needs access to the controlling terminal.
fn needs_terminal_device(input_is_terminal: bool, output_is_terminal: bool) -> bool {
    input_is_terminal || output_is_terminal
}

/// Probe only when standard output is the terminal that will receive images.
fn resolve_graphics_support_for_output(
    output_is_terminal: bool,
    probe: impl FnOnce() -> io::Result<TerminalProbe>,
) -> io::Result<TerminalProbe> {
    if output_is_terminal {
        probe()
    } else {
        Ok(TerminalProbe::unsupported())
    }
}

/// Run one terminal operation between raw-mode entry and cooked-mode restore.
fn run_with_raw_mode<Terminal, OperationOutput>(
    terminal: &mut Terminal,
    enter_raw: impl FnOnce(&mut Terminal) -> io::Result<()>,
    operation: impl FnOnce(&mut Terminal) -> io::Result<OperationOutput>,
    enter_cooked: impl FnOnce(&mut Terminal) -> io::Result<()>,
) -> io::Result<OperationOutput> {
    enter_raw(terminal)?;
    let operation_result = operation(terminal);
    let restore_result = enter_cooked(terminal);
    match (operation_result, restore_result) {
        (Err(operation_error), Err(restore_error)) => {
            tracing::warn!(%restore_error, "could not restore terminal after failed operation");
            Err(operation_error)
        }
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Ok(operation_output), Ok(())) => Ok(operation_output),
    }
}

/// Lock the shared terminal and recover its value after a poisoned lock.
fn lock_terminal(
    terminal_mutex: &Mutex<Option<TerminalDevice>>,
) -> std::sync::MutexGuard<'_, Option<TerminalDevice>> {
    terminal_mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restore the shared terminal once, waiting for an in-progress terminal write.
fn restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let terminal_device = lock_terminal(shared_terminal).take();
    let Some(mut terminal) = terminal_device else {
        return;
    };
    restore_terminal(
        &mut terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore without blocking a panic hook on the thread that holds the terminal.
fn try_restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut terminal_guard = match shared_terminal.try_lock() {
        Ok(terminal_guard) => terminal_guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            write_fallback_terminal_cleanup(
                graphics_support,
                application_modes_active,
                image_cleanup_claimed,
            );
            return;
        }
    };
    let Some(mut terminal) = terminal_guard.take() else {
        return;
    };
    drop(terminal_guard);
    restore_terminal(
        &mut terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore application modes and the platform's cooked terminal mode.
fn restore_terminal(
    terminal: &mut TerminalDevice,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    if let Err(application_restore_error) = restore_application_modes(
        terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    ) {
        tracing::warn!(%application_restore_error, "could not restore terminal application modes");
    }
    if let Err(cooked_mode_error) = terminal.enter_cooked_mode() {
        tracing::warn!(%cooked_mode_error, "could not restore terminal cooked mode");
    }
}

/// Claim and restore this attachment's application-level terminal modes once.
fn restore_application_modes<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    if !application_modes_active.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    write_terminal_cleanup(writer, graphics_support, image_cleanup_claimed)
}

/// Claim the one Kitty cleanup allowed for an attachment.
fn claim_image_cleanup(image_cleanup_claimed: &AtomicBool) -> bool {
    !image_cleanup_claimed.swap(true, Ordering::AcqRel)
}

/// Gather every capability reply available before one shared deadline.
fn probe_terminal<EventSourceType: reader::EventSource, OutputWriter: Write>(
    writer: &mut OutputWriter,
    reader: &mut InputReader<EventSourceType>,
) -> io::Result<TerminalProbe> {
    write_terminal_probe_queries(writer)?;
    let probe_deadline = Instant::now() + TERMINAL_QUERY_TIMEOUT_DURATION;
    let mut probe_replies = ProbeReplies::default();
    loop {
        let remaining_probe_timeout = probe_deadline.saturating_duration_since(Instant::now());
        if !reader.wait_for_event(Some(remaining_probe_timeout), is_probe_event)? {
            break;
        }
        let probe_event = reader.read_matching_event(is_probe_event)?;
        probe_replies.observe(probe_event);
        if Instant::now() >= probe_deadline {
            break;
        }
    }
    Ok(probe_replies.build_terminal_probe())
}

/// Replies collected while capability queries share one deadline.
#[derive(Default)]
struct ProbeReplies {
    supports_kitty: bool,
    supports_iterm_file_output: bool,
    supports_iterm_sixel: bool,
    supports_da1_sixel: bool,
    sixel_palette_color_count: Option<Result<u32, koshi_input::host::GraphicAttributeError>>,
    sixel_geometry: Option<Result<(u32, u32), koshi_input::host::GraphicAttributeError>>,
    cell_size: Option<PixelCellSize>,
}

impl ProbeReplies {
    /// Retain one parsed event that belongs to the capability probe.
    fn observe(&mut self, probe_event: Event) {
        match probe_event {
            Event::KittyGraphicsReply(kitty_reply)
                if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID =>
            {
                self.supports_kitty |= kitty_reply.is_successful;
            }
            Event::TerminalFeatures(terminal_features) => {
                self.supports_iterm_file_output |=
                    iterm_feature_string_supports_file(&terminal_features);
                self.supports_iterm_sixel |=
                    iterm_feature_string_supports_sixel(&terminal_features);
            }
            Event::PrimaryDeviceAttributes(device_attributes) => {
                self.supports_da1_sixel |= device_attributes
                    .get(1..)
                    .is_some_and(|attribute_numbers| attribute_numbers.contains(&4));
            }
            Event::SixelGraphicsAttributeReply(sixel_attribute_reply) => {
                match sixel_attribute_reply {
                    koshi_input::host::GraphicAttributeReply::Palette(palette_color_count) => {
                        if self.sixel_palette_color_count.is_none() {
                            self.sixel_palette_color_count = Some(palette_color_count);
                        }
                    }
                    koshi_input::host::GraphicAttributeReply::Geometry(pixel_dimensions) => {
                        if self.sixel_geometry.is_none() {
                            self.sixel_geometry = Some(pixel_dimensions);
                        }
                    }
                }
            }
            Event::CellSize(cell_size) => {
                if self.cell_size.is_none() {
                    self.cell_size = Some(cell_size);
                }
            }
            Event::KittyGraphicsReply(_)
            | Event::Key(_)
            | Event::Mouse(_)
            | Event::WindowResized(_)
            | Event::Paste(_)
            | Event::FocusIn
            | Event::FocusOut
            | Event::KeyboardEnhancementFlags(_) => {}
        }
    }

    /// Select the preferred protocol and retain the measured cell size.
    fn build_terminal_probe(self) -> TerminalProbe {
        let graphics_support = if self.supports_kitty {
            GraphicsSupport::Kitty
        } else if self.supports_iterm_file_output {
            GraphicsSupport::Iterm
        } else {
            self.build_sixel_graphics_support()
                .unwrap_or(GraphicsSupport::Unsupported)
        };
        TerminalProbe {
            graphics_support,
            cell_size: self.cell_size,
        }
    }

    /// Build bounded Sixel support from the terminal's positive evidence.
    fn build_sixel_graphics_support(&self) -> Option<GraphicsSupport> {
        let is_sixel_advertised = self.supports_da1_sixel
            || self.supports_iterm_sixel
            || matches!(self.sixel_geometry, Some(Ok(_)));
        if !is_sixel_advertised {
            return None;
        }
        let palette_color_count = match self.sixel_palette_color_count {
            Some(Ok(palette_color_count)) if palette_color_count < 2 => return None,
            Some(Ok(palette_color_count)) => palette_color_count.min(256) as usize,
            Some(Err(_)) | None => 2,
        };
        let (max_pixel_width, max_pixel_height) = match self.sixel_geometry {
            Some(Ok((pixel_width, pixel_height))) => (
                (pixel_width != 0).then_some(pixel_width),
                (pixel_height != 0).then_some(pixel_height),
            ),
            Some(Err(_)) | None => (None, None),
        };
        Some(GraphicsSupport::Sixel {
            palette_color_count,
            max_pixel_width,
            max_pixel_height,
        })
    }
}

/// Write every bounded capability query and flush them as one probe batch.
fn write_terminal_probe_queries<W: Write>(writer: &mut W) -> io::Result<()> {
    write_kitty_support_query(writer).map_err(convert_kitty_output_error)?;
    writer.write_all(ITERM_CAPABILITIES_QUERY)?;
    writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    writer.write_all(SIXEL_PALETTE_QUERY)?;
    writer.write_all(SIXEL_GEOMETRY_QUERY)?;
    writer.write_all(CELL_SIZE_QUERY_BYTES)?;
    writer.flush()
}

/// Return whether an event belongs to the capability probe.
fn is_probe_event(probe_event: &Event) -> bool {
    matches!(probe_event, Event::KittyGraphicsReply(kitty_reply) if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID)
        || matches!(probe_event, Event::TerminalFeatures(_))
        || matches!(probe_event, Event::PrimaryDeviceAttributes(_))
        || matches!(probe_event, Event::SixelGraphicsAttributeReply(_))
        || matches!(probe_event, Event::CellSize(_))
}

/// Request the terminal modes used by the attached viewer.
fn enable_terminal_modes<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
) -> io::Result<()> {
    if matches!(graphics_support, GraphicsSupport::Sixel { .. }) {
        writer.write_all(image_output::get_sixel_mode_save_bytes())?;
    }
    writer.write_all(APPLICATION_MODE_SETUP_BYTES)?;
    writer.write_all(KEYBOARD_ENHANCEMENT_QUERY_BYTES)?;
    writer.flush()
}

/// Write every application-level terminal reset in reverse setup order.
fn write_terminal_cleanup<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    let mut first_io_error = None;
    if matches!(graphics_support, GraphicsSupport::Kitty)
        && claim_image_cleanup(image_cleanup_claimed)
    {
        retain_first_io_error(
            &mut first_io_error,
            write_kitty_abort(writer).map_err(convert_kitty_output_error),
        );
        retain_first_io_error(
            &mut first_io_error,
            write_kitty_delete_all(writer).map_err(convert_kitty_output_error),
        );
    }
    if matches!(graphics_support, GraphicsSupport::Sixel { .. }) {
        retain_first_io_error(&mut first_io_error, image_output::write_image_abort(writer));
        retain_first_io_error(
            &mut first_io_error,
            writer.write_all(image_output::get_sixel_mode_restore_bytes()),
        );
    }
    retain_first_io_error(
        &mut first_io_error,
        writer.write_all(APPLICATION_MODE_CLEANUP_BYTES),
    );
    retain_first_io_error(&mut first_io_error, writer.flush());
    match first_io_error {
        Some(io_error) => Err(io_error),
        None => Ok(()),
    }
}

/// Keep the first terminal I/O failure while cleanup attempts every reset.
fn retain_first_io_error(first_io_error: &mut Option<io::Error>, io_result: io::Result<()>) {
    if let Err(io_error) = io_result {
        if first_io_error.is_none() {
            *first_io_error = Some(io_error);
        }
    }
}

/// Write application resets when the panic hook cannot take the terminal lock.
fn write_fallback_terminal_cleanup(
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut stdout = io::stdout();
    let _ = restore_application_modes(
        &mut stdout,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Read semantic terminal events until shutdown or input failure.
fn run_terminal_input(
    input_reader: &mut InputReader,
    runtime_event_sender: &mpsc::SyncSender<RuntimeEvent>,
    client_id: ClientId,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Acquire) {
        let runtime_event = match input_reader.read_matching_event(|_| true) {
            Ok(host_event) => build_terminal_runtime_event(client_id, host_event),
            Err(input_read_error)
                if input_read_error.kind() == io::ErrorKind::Interrupted
                    && shutdown.load(Ordering::Acquire) =>
            {
                break;
            }
            Err(input_read_error) if input_read_error.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(input_read_error) => {
                tracing::warn!(%input_read_error, "could not read terminal input");
                Some(RuntimeEvent::Quit)
            }
        };
        if let Some(runtime_event) = runtime_event {
            let is_quit_event = matches!(runtime_event, RuntimeEvent::Quit);
            if runtime_event_sender.send(runtime_event).is_err() || is_quit_event {
                break;
            }
        }
    }
}

/// Convert one host-terminal event into the runtime event the viewer consumes.
fn build_terminal_runtime_event(client_id: ClientId, host_event: Event) -> Option<RuntimeEvent> {
    match host_event {
        Event::Key(host_key_event) => Some(RuntimeEvent::KeyInput {
            client_id,
            key_input: decode_key_event(host_key_event),
        }),
        Event::CellSize(cell_size) => Some(RuntimeEvent::CellSize {
            client_id,
            cell_size,
        }),
        Event::WindowResized(window_size) => {
            Some(build_resize_runtime_event(client_id, window_size))
        }
        Event::Mouse(mouse_event) => Some(RuntimeEvent::MouseInput {
            client_id,
            mouse_input: decode_mouse(mouse_event),
        }),
        Event::Paste(pasted_text) => Some(RuntimeEvent::HostPaste {
            client_id,
            pasted_text,
        }),
        Event::KeyboardEnhancementFlags(enhancement_flags) => {
            tracing::debug!(
                enhancement_flags,
                "the terminal reported the keyboard enhancements it applied"
            );
            None
        }
        Event::FocusIn
        | Event::PrimaryDeviceAttributes(_)
        | Event::TerminalFeatures(_)
        | Event::SixelGraphicsAttributeReply(_)
        | Event::KittyGraphicsReply(_) => None,
        Event::FocusOut => Some(RuntimeEvent::OuterTerminalFocusLost { client_id }),
    }
}

/// Build the runtime resize event for one host size report.
fn build_resize_runtime_event(client_id: ClientId, window_size: WindowSize) -> RuntimeEvent {
    let viewport_size = Size {
        column_count: window_size.column_count,
        row_count: window_size.row_count,
    };
    RuntimeEvent::Resize {
        client_id,
        viewport_size,
        pane_area: Some(crate::compute_core_pane_area(viewport_size)),
        cell_size: compute_pixel_cell_size(window_size),
    }
}

/// Read one cell's pixel dimensions from the output terminal's window size.
///
/// `None` when the window size cannot be read, when the platform reports no
/// pixel dimensions, or when they do not divide evenly across the grid.
pub(crate) fn read_local_cell_size() -> Option<PixelCellSize> {
    platform::read_window_size()
        .ok()
        .and_then(compute_pixel_cell_size)
}

/// Derive one cell's pixel dimensions from a host window report only when the
/// complete pixel dimensions divide evenly across the reported grid.
fn compute_pixel_cell_size(window_size: WindowSize) -> Option<PixelCellSize> {
    let pixel_width = window_size.pixel_width?;
    let pixel_height = window_size.pixel_height?;
    if window_size.column_count == 0
        || window_size.row_count == 0
        || pixel_width % window_size.column_count != 0
        || pixel_height % window_size.row_count != 0
    {
        return None;
    }
    PixelCellSize::from_pixel_dimensions(
        pixel_width / window_size.column_count,
        pixel_height / window_size.row_count,
    )
}

/// Build the viewer half and apply `loaded_config`'s viewer-owned files, in one step.
///
/// `client_id` is the id this viewer's input events and commands carry.
/// `viewport_size` is this terminal's size in cells. `frame_delivery_receiver` is the frame feed; a
/// client owns no session, so the receiver it is handed has no sender and its
/// frames arrive over the connection instead. `terminal_cleanup_guard` is the guard that
/// restores the outer terminal.
///
/// `loaded_config.app_config_layer` and `loaded_config.theme_config_layer` fold into the viewer's settings and chrome
/// colors and always apply. `loaded_config.keybindings` is validated: a verdict other
/// than [`Apply`](koshi_config::conflict::KeymapVerdict::Apply) logs a warning
/// naming `koshi keys conflicts`, an `Apply` logs `"keybinding.kdl applied"`,
/// and a `None` keymap layer logs nothing.
pub(crate) fn build_client_with_loaded_config(
    client_id: ClientId,
    viewport_size: Size,
    frame_delivery_receiver: mpsc::Receiver<koshi_renderer::snapshot::Delivery>,
    terminal_cleanup_guard: TerminalCleanupGuard,
    loaded_config: koshi_link::config::LoadedConfig,
) -> Client {
    let mut viewer_client = Client::from_client_id_and_viewport(
        client_id,
        viewport_size,
        frame_delivery_receiver,
        terminal_cleanup_guard,
    );
    match viewer_client.load_startup_config(
        loaded_config.app_config_layer,
        loaded_config.theme_config_layer,
        loaded_config.keybindings,
    ) {
        Some(report) if report.get_verdict() != koshi_config::conflict::KeymapVerdict::Apply => {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }
    viewer_client
}

/// Draw `snapshot` into `terminal`, keeping the outer terminal's window title
/// and cursor style in step with the focused pane.
///
/// The theme comes from `client`, and so does the hint bar, built for the
/// client's active base or placement input mode and
/// `frame_paint.is_mouse_selection_enabled`. The hovered pane, the tab
/// strip's position and the open key sequence come from `frame_paint`.
/// `committed_regions` is the geometry shared by the painter and cursor
/// placement for this frame.
///
/// `last_window_title` and `last_cursor_style` store the title and cursor style used to
/// decide whether the next frame needs a control write. Both are read before
/// the buffer paint and updated after it succeeds: a changed title writes
/// `SetTitle`, and a changed cursor style writes `SetCursorStyle`. A frame
/// that [`get_cursor_style`] names no style stores `None` and writes no style
/// command.
///
/// # Errors
///
/// Returns a backend or native-image error when the frame cannot be fully
/// painted. A failed title or cursor-style write is ignored.
#[cfg(test)]
pub(crate) fn paint_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let mut image_output_state = ImageOutputState::disabled();
    paint_frame_with_images(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        &mut image_output_state,
        None,
        last_window_title,
        last_cursor_style,
        None,
        None,
    )
    .map(|_| ())
}

/// Paint one frame and schedule native images when the outer terminal supports
/// them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frame_with_images<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
    placement_snapshot: Option<&PlacementSnapshot>,
    placement_display_snapshot: Option<&PlacementSnapshot>,
) -> Result<bool, PaintError<B::Error>> {
    let mut stdout = io::stdout();
    paint_frame_with_writer(
        &mut stdout,
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        image_mode,
        image_output_state,
        cell_size,
        last_window_title,
        last_cursor_style,
        placement_snapshot,
        placement_display_snapshot,
    )
}

/// Paint one frame and send terminal-control output to `writer`.
#[allow(clippy::too_many_arguments)]
fn paint_frame_with_writer<B: Backend, W: Write>(
    writer: &mut W,
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
    placement_snapshot: Option<&PlacementSnapshot>,
    placement_display_snapshot: Option<&PlacementSnapshot>,
) -> Result<bool, PaintError<B::Error>> {
    let window_title_text = build_window_title(snapshot);
    let is_window_title_changed = window_title_text != *last_window_title;
    let cursor_style = get_cursor_style(snapshot);
    let is_cursor_style_changed = cursor_style != *last_cursor_style;
    let keymap_hints = client.build_frame_hints(
        frame_paint.lock_mode,
        frame_paint.is_mouse_selection_enabled,
    );
    let terminal_size = terminal.size().map_err(PaintError::Backend)?;
    let mut render_area = Rect::new(0, 0, terminal_size.width, terminal_size.height);
    let mut hardware_cursor_position =
        get_cursor_position(snapshot, committed_regions, render_area);
    let is_native_image_output = image_output_state.output_kind().is_some();
    image_output_state.set_host_terminal_size(terminal_size.width, terminal_size.height);
    let mut image_paint_commands = build_image_paints(snapshot, committed_regions, render_area);
    if let Some(placement_snapshot) = placement_snapshot.or(placement_display_snapshot) {
        let placement_display_snapshot = placement_display_snapshot.unwrap_or(placement_snapshot);
        image_paint_commands.extend(build_placement_image_paints(
            placement_display_snapshot,
            render_area,
        ));
    }
    let placement_composition_buffer = image_output_state
        .output_kind()
        .filter(|output_kind| {
            output_kind.uses_cell_composition() && !image_paint_commands.is_empty()
        })
        .and_then(|_| {
            placement_snapshot
                .or(placement_display_snapshot)
                .and_then(|placement_snapshot| {
                    let placement_display_snapshot =
                        placement_display_snapshot.unwrap_or(placement_snapshot);
                    build_placement_composition_buffer(
                        placement_snapshot,
                        placement_display_snapshot,
                        frame_paint.placement_target.as_ref(),
                        client.get_theme(),
                        &image_paint_commands,
                        render_area,
                    )
                })
        });
    let image_cell_composition_snapshot = image_output_state
        .output_kind()
        .filter(|output_kind| {
            output_kind.uses_cell_composition() && !image_paint_commands.is_empty()
        })
        .and_then(|_| {
            let mut cell_snapshot =
                build_image_cell_snapshot(snapshot, committed_regions, render_area)?;
            if let Some((placement_buffer, panel_rect)) = placement_composition_buffer.as_ref() {
                cell_snapshot.overlay_buffer(*panel_rect, placement_buffer);
            }
            Some(Arc::new(cell_snapshot))
        });
    if is_native_image_output
        && !image_output_state.prepare_frame(
            &image_paint_commands,
            image_cell_composition_snapshot,
            cell_size,
        )
    {
        return Ok(false);
    }
    let is_native_image_commit =
        is_native_image_output && image_output_state.native_commit_pending();
    let native_image_output_bytes = if is_native_image_commit {
        match image_output_state.frame_output(hardware_cursor_position) {
            Ok(native_image_output_bytes) => native_image_output_bytes,
            Err(image_frame_error) => {
                image_output_state.fail_frame_commit();
                return Err(PaintError::Image(image_frame_error));
            }
        }
    } else {
        Vec::new()
    };
    if is_native_image_commit {
        if let Err(synchronized_update_error) = execute!(writer, BeginSynchronizedUpdate) {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(PaintError::Image(synchronized_update_error));
        }
    }
    let paint_result = (|| {
        if is_native_image_output {
            if tracing::enabled!(tracing::Level::DEBUG) {
                for image_placement_key in image_output_state
                    .list_prepared_placement_keys()
                    .iter()
                    .copied()
                {
                    if let Some(image_compatibility) =
                        image_output_state.get_image_compatibility(image_placement_key)
                    {
                        if image_compatibility != ImageCompatibility::default() {
                            tracing::debug!(
                                ?image_placement_key,
                                ?image_compatibility,
                                "native image output has host compatibility limits"
                            );
                        }
                    }
                }
            }
            if image_output_state
                .write_frame_reset(writer)
                .map_err(PaintError::Image)?
            {
                // The host screen is blank after `ESC[2J`. Emptying both ratatui
                // buffers makes the next draw write every cell again.
                terminal.swap_buffers();
            }
        }
        let available_image_placement_keys =
            is_native_image_output.then(|| image_output_state.list_prepared_placement_keys());
        let prepared_preview_image_keys =
            is_native_image_output.then(|| image_output_state.list_prepared_preview_image_keys());
        terminal
            .draw(|render_frame| {
                let frame_area = render_frame.area();
                render_area = frame_area;
                hardware_cursor_position =
                    get_cursor_position(snapshot, committed_regions, frame_area);
                render_frame.render_widget(
                    SnapshotWidget {
                        snapshot,
                        theme: client.get_theme(),
                        hints: &keymap_hints,
                        pending_key_sequence: frame_paint.pending_key_sequence.as_ref(),
                        chrome: frame_paint.chrome,
                        committed_regions,
                        image_mode,
                        available_image_placement_keys,
                        prepared_preview_image_keys,
                        placement_snapshot,
                        placement_display_snapshot,
                        placement_target: frame_paint.placement_target.as_ref(),
                        placement_status: frame_paint.placement_status.as_ref(),
                    },
                    frame_area,
                );
                if let Some(cursor_position) = hardware_cursor_position {
                    render_frame.set_cursor_position(cursor_position);
                }
            })
            .map_err(PaintError::Backend)?;
        if is_window_title_changed {
            execute!(writer, SetTitle(&window_title_text)).map_err(PaintError::Image)?;
        }
        if is_cursor_style_changed {
            if let Some(cursor_command) = cursor_style.map(set_cursor_style) {
                execute!(writer, cursor_command).map_err(PaintError::Image)?;
            }
        }
        if is_native_image_commit {
            writer
                .write_all(&native_image_output_bytes)
                .map_err(PaintError::Image)?;
        }
        Ok(())
    })();
    if is_native_image_commit {
        if let Err(frame_paint_error) = paint_result {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(frame_paint_error);
        }
        if let Err(synchronized_flush_error) =
            execute!(writer, EndSynchronizedUpdate).and_then(|()| writer.flush())
        {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(PaintError::Image(synchronized_flush_error));
        }
        image_output_state.commit_frame();
    } else {
        paint_result?;
    }
    if is_window_title_changed {
        *last_window_title = window_title_text;
    }
    if is_cursor_style_changed {
        *last_cursor_style = cursor_style;
    }
    Ok(true)
}

/// Cancel an incomplete terminal control string and close synchronized output.
fn recover_synchronized_frame<W: Write>(writer: &mut W) {
    let _ = image_output::write_image_abort(writer);
    let _ = execute!(writer, EndSynchronizedUpdate);
    let _ = writer.flush();
}

fn restore_cursor_state<W: Write>(
    writer: &mut W,
    cursor_position: Option<ratatui::layout::Position>,
) -> io::Result<()> {
    if let Some(cursor_position) = cursor_position {
        write!(
            writer,
            "\x1b[{};{}H",
            u32::from(cursor_position.y) + 1,
            u32::from(cursor_position.x) + 1
        )?;
    } else {
        writer.write_all(b"\x1b[?25l")?;
    }
    Ok(())
}

fn convert_kitty_output_error(kitty_error: KittyOutputError) -> io::Error {
    match kitty_error {
        KittyOutputError::Io(io_error) => io_error,
        kitty_error => io::Error::new(io::ErrorKind::InvalidData, kitty_error),
    }
}

/// The crossterm command for one pane's cursor style.
///
/// Each [`Shaped`](CursorStyle::Shaped) shape-and-blink pair maps to the one
/// crossterm variant that re-emits the same DECSCUSR sequence:
/// `Shaped { shape: Bar, blink: true }` results in
/// [`BlinkingBar`](SetCursorStyle::BlinkingBar).
/// [`UserDefault`](CursorStyle::UserDefault) maps to
/// [`DefaultUserShape`](SetCursorStyle::DefaultUserShape), which hands the
/// cursor back to whatever the user configured in their own terminal.
pub(crate) fn set_cursor_style(cursor_style: CursorStyle) -> SetCursorStyle {
    let CursorStyle::Shaped { shape, blink } = cursor_style else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (shape, blink) {
        (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
        (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
        (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
    }
}

/// The outer emulator's window title for one frame: the session name, plus
/// `" | "` and the focused pane's resolved title when that pane is in
/// `render_snapshot.pane_snapshots` and its title is a non-empty string.
///
/// Session `"quiet-lake"` with the focused pane titled `"htop"` results in
/// `"quiet-lake | htop"`. No focused pane, a focused pane missing from
/// `render_snapshot.pane_snapshots`, no title, or an empty title all result in
/// `"quiet-lake"`.
pub(crate) fn build_window_title(render_snapshot: &RenderSnapshot) -> String {
    let focused_pane_title = render_snapshot
        .client_snapshot
        .focused_pane_id
        .and_then(|focused_pane_id| {
            render_snapshot
                .pane_snapshots
                .iter()
                .find(|pane_snapshot| pane_snapshot.pane_id == focused_pane_id)
        })
        .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref());
    match focused_pane_title {
        Some(pane_title) if !pane_title.is_empty() => {
            format!(
                "{} | {pane_title}",
                render_snapshot.session_snapshot.session_name
            )
        }
        _ => render_snapshot.session_snapshot.session_name.clone(),
    }
}

#[cfg(test)]
mod tests;
