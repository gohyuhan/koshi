//! The outer terminal an attached client owns: the viewer built for it, the
//! thread that reads its input, and painting frames into it.
//!
//! Every item here belongs to one attached terminal. The session it is joined
//! to owns none of them.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::Widget;

use crate::attach::ViewerPaint;
use crate::input::InputReadClock;
use crate::{is_focused_pane_floating, Client};
use koshi_core::command::{PanePlacementAnchor, TiledPlacementTarget};
use koshi_core::constant::limit_viewport_size;
use koshi_core::geometry::{PixelCellSize, Point, Rect as CoreRect, Size};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::KeySequence;
use koshi_input::host::{Event, WindowSize};
use koshi_input::keyboard::decode_key_event;
use koshi_input::mouse::decode_mouse;
use koshi_ipc::protocol::GraphicsCapabilities;
use koshi_iterm::{
    supports_iterm_file_feature, supports_iterm_sixel_feature, ITERM_CAPABILITIES_QUERY,
};
use koshi_kitty::{
    write_kitty_abort, write_kitty_delete_all, write_kitty_support_query, KittyOutputError,
    KITTY_QUERY_IMAGE_ID,
};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, FloatingPaneSlot, KeymapHints, PanePlacementPresentation,
    PaneSnapshot, PlacementSnapshot, PlacementTabSnapshot, RenderSnapshot, TabSnapshot,
    ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    build_image_cell_snapshot, build_image_paints, get_cursor_position, get_cursor_style,
    list_changed_screen_rows, render_frame, DrawnRows, ImagePlacementKey, ImageRenderMode,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_sixel::{PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY};
use koshi_terminal::state::CursorShape;

use self::platform::{PlatformWaker, TerminalDevice};
use self::reader::InputReader;

mod image_output;
mod paint_driver;
mod platform;
mod reader;

pub(crate) use self::image_output::{ImageOutputKind, ImageOutputState};
pub(crate) use self::paint_driver::{PaintDriver, TerminalOutput, TerminalOutputBackend};

const TERMINAL_QUERY_TIMEOUT_DURATION: Duration = Duration::from_millis(300);

/// Request a terminal's current cell dimensions in pixels.
const CELL_SIZE_QUERY_BYTES: &[u8] = b"\x1b[16t";

/// Enter the alternate screen and enable keyboard, mouse, paste, and focus
/// reports.
///
/// The keyboard push asks for flags `1|2|4|8|16`: disambiguate escape codes,
/// report event types, report alternate keys, report all keys as escape codes,
/// and report associated text. Every one of those reaches a pane through the
/// event Koshi sends, encoded for that pane's own flags.
///
/// Flags `8` and `16` together move ordinary typing into `CSI u` reports that
/// carry the text the key produced: typing `å` reaches a pane as `å`.
///
/// A terminal that ignores the push keeps reporting legacy bytes, and every
/// key reaches a pane as those legacy bytes.
///
/// Focus reports (`CSI ? 1004 h`) make the terminal send `CSI I` when its
/// window gains focus and `CSI O` when it loses focus.
const APPLICATION_MODE_SETUP_BYTES: &[u8] =
    b"\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h";

const SHIFT_MOUSE_CAPTURE_REQUEST_BYTES: &[u8] = b"\x1b[>1s";
const SHIFT_MOUSE_CAPTURE_RELEASE_BYTES: &[u8] = b"\x1b[>0s";

/// Ask which Kitty keyboard enhancements the terminal applied. The answer is
/// `ESC [ ? flags u`.
const KEYBOARD_ENHANCEMENT_QUERY_BYTES: &[u8] = b"\x1b[?u";

/// Reset Shift mouse capture, disable focus, paste, mouse, keyboard, and
/// alternate-screen modes, and restore cursor state.
const APPLICATION_MODE_CLEANUP_BYTES: &[u8] =
    b"\x1b[>0s\x1b[?1004l\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q";

/// Paints a render snapshot into ratatui's frame buffer with
/// [`koshi_renderer::render_frame`]. With a placement preview, a displayed
/// placement tab and a placement target, it then outlines the target's slot
/// in the preview's displayed tab.
pub(crate) struct SnapshotWidget<'a> {
    /// The frame the session handed out.
    pub(crate) render_snapshot: &'a RenderSnapshot,
    /// The colors this viewer paints koshi's chrome in.
    pub(crate) theme: &'a Theme,
    /// The hint-bar data for the mode this viewer is in.
    pub(crate) keymap_hints: &'a KeymapHints,
    /// The multi-chord sequence this viewer has open.
    pub(crate) pending_key_sequence: Option<&'a KeySequence>,
    /// The pane this viewer's pointer is over, and where its tab strip sits.
    pub(crate) viewer_chrome: ViewerChrome,
    /// The region solve committed with the frame being painted.
    pub(crate) committed_regions: &'a CommittedRegions,
    /// The image mode selected for this outer terminal.
    pub(crate) image_mode: ImageRenderMode,
    /// The image placements that show the unavailable-image text in native
    /// image mode.
    pub(crate) unavailable_image_placement_keys: &'a [ImagePlacementKey],
    /// The layouts of the placement preview the paint draws from.
    pub(crate) placement_snapshot: Option<&'a PlacementSnapshot>,
    /// The tab the placement preview shows, at the current animation time.
    pub(crate) placement_display_tab_snapshot: Option<&'a PlacementTabSnapshot>,
    /// The target selected by the viewer's placement interaction.
    pub(crate) placement_target: Option<&'a TiledPlacementTarget>,
    /// The whole-pane visual explanation for the viewer's placement.
    pub(crate) placement_presentation: Option<&'a PanePlacementPresentation>,
    /// The rows of the buffer the frame draws.
    pub(crate) drawn_rows: DrawnRows<'a>,
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, render_area: Rect, render_buffer: &mut Buffer) {
        render_frame(
            self.render_snapshot,
            self.committed_regions,
            self.theme,
            self.keymap_hints,
            self.pending_key_sequence,
            self.viewer_chrome,
            self.image_mode,
            self.unavailable_image_placement_keys,
            self.placement_presentation,
            self.placement_target,
            render_area,
            self.drawn_rows,
            render_buffer,
        );
        if let (
            Some(placement_snapshot),
            Some(placement_display_tab_snapshot),
            Some(placement_target),
        ) = (
            self.placement_snapshot,
            self.placement_display_tab_snapshot,
            self.placement_target,
        ) {
            draw_placement_target_outline(
                placement_snapshot,
                placement_display_tab_snapshot,
                self.render_snapshot,
                placement_target,
                self.theme,
                self.committed_regions,
                render_area,
                render_buffer,
            );
        }
    }
}

/// Build the frame shown for a placement tab: `render_snapshot` with
/// `displayed_tab_snapshot` as its active tab, focused on `source_pane_id`
/// when that tab shows it. Each pane of that tab and each floating pane keeps
/// the content `render_snapshot` carries for it, and a pane it carries none
/// for is drawn empty. A drag previewing tab `work` returns `work` with its
/// proposed pane slots. The floating pane slots stay as `render_snapshot`
/// holds them.
pub(crate) fn build_placement_render_snapshot(
    render_snapshot: &RenderSnapshot,
    displayed_tab_snapshot: &PlacementTabSnapshot,
    source_pane_id: PaneId,
) -> RenderSnapshot {
    let mut placement_render_snapshot = render_snapshot.clone();
    let displayed_tab_id = displayed_tab_snapshot.tab_snapshot.tab_id;
    placement_render_snapshot
        .session_snapshot
        .active_tab_snapshot = displayed_tab_snapshot.tab_snapshot.clone();
    placement_render_snapshot.client_snapshot.active_tab_id = displayed_tab_id;
    placement_render_snapshot.client_snapshot.focused_pane_id = displayed_tab_snapshot
        .tab_snapshot
        .find_visible_pane_slot(source_pane_id)
        .map(|_| source_pane_id);
    for tab_metadata in &mut placement_render_snapshot.session_snapshot.tabs_metadata {
        tab_metadata.is_active = tab_metadata.tab_id == displayed_tab_id;
    }
    let displayed_pane_ids = displayed_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .chain(
            render_snapshot
                .session_snapshot
                .floating_pane_slots
                .iter()
                .map(|floating_pane_slot| floating_pane_slot.pane_id),
        )
        .collect::<HashSet<_>>();
    placement_render_snapshot
        .pane_snapshots
        .retain(|pane_snapshot| displayed_pane_ids.contains(&pane_snapshot.pane_id));
    let content_pane_ids = placement_render_snapshot
        .pane_snapshots
        .iter()
        .map(|pane_snapshot| pane_snapshot.pane_id)
        .collect::<HashSet<_>>();
    for pane_slot in &displayed_tab_snapshot.tab_snapshot.pane_slots {
        if !content_pane_ids.contains(&pane_slot.pane_id) {
            placement_render_snapshot
                .pane_snapshots
                .push(PaneSnapshot::from_empty_pane(pane_slot.pane_id));
        }
    }
    placement_render_snapshot
}

/// Build the frame shown while a committed placement or a floated pane
/// slides: `render_snapshot` with `animated_tab_snapshot`, when `Some`, as its
/// active tab, and with each of `animated_floating_pane_slots` in place of the
/// floating pane slot that names the same pane.
pub(crate) fn build_committed_placement_render_snapshot(
    render_snapshot: &RenderSnapshot,
    animated_tab_snapshot: Option<TabSnapshot>,
    animated_floating_pane_slots: &[FloatingPaneSlot],
) -> RenderSnapshot {
    let mut committed_placement_render_snapshot = render_snapshot.clone();
    if let Some(animated_tab_snapshot) = animated_tab_snapshot {
        committed_placement_render_snapshot
            .session_snapshot
            .active_tab_snapshot = animated_tab_snapshot;
    }
    for floating_pane_slot in &mut committed_placement_render_snapshot
        .session_snapshot
        .floating_pane_slots
    {
        let animated_floating_pane_slot =
            animated_floating_pane_slots
                .iter()
                .find(|animated_floating_pane_slot| {
                    animated_floating_pane_slot.pane_id == floating_pane_slot.pane_id
                });
        if let Some(animated_floating_pane_slot) = animated_floating_pane_slot {
            *floating_pane_slot = *animated_floating_pane_slot;
        }
    }
    committed_placement_render_snapshot
}

/// The outer box of `pane_id` in `tab_snapshot`, counted from the origin of a
/// pane area of `pane_area_size` that draws the tab centered: the tab's slot
/// moved by the tab's letterbox offset. `None` when the tab shows no content
/// for the pane.
///
/// A `40x22` slot at `(40, 0)` in an `80x22` tab drawn in a `100x30` pane area
/// gives `(50, 4)` `40x22`.
pub(crate) fn find_tiled_pane_rect_in_pane_area(
    tab_snapshot: &TabSnapshot,
    pane_area_size: Size,
    pane_id: PaneId,
) -> Option<CoreRect> {
    let pane_slot = tab_snapshot.find_visible_pane_slot(pane_id)?;
    let tab_area = koshi_renderer::compute_content_rect(
        Rect {
            x: 0,
            y: 0,
            width: pane_area_size.column_count,
            height: pane_area_size.row_count,
        },
        tab_snapshot.tab_size,
    );
    Some(CoreRect::from_origin_and_size(
        Point {
            column: pane_slot
                .outer_rect
                .origin
                .column
                .saturating_add(tab_area.x),
            row: pane_slot.outer_rect.origin.row.saturating_add(tab_area.y),
        },
        pane_slot.outer_rect.size,
    ))
}

/// Draw the target border at the destination pane's original slot in
/// `placement_snapshot`'s displayed tab, placed by the size that tab is
/// solved at. If `pane-123` swaps with `pane-456`, the outline stays at
/// `pane-456`'s original slot while `pane-123` animates there.
#[allow(clippy::too_many_arguments)]
fn draw_placement_target_outline(
    placement_snapshot: &PlacementSnapshot,
    placement_display_tab_snapshot: &PlacementTabSnapshot,
    displayed_render_snapshot: &RenderSnapshot,
    placement_target: &TiledPlacementTarget,
    theme: &Theme,
    committed_regions: &CommittedRegions,
    render_area: Rect,
    render_buffer: &mut Buffer,
) {
    if let TiledPlacementTarget::Split {
        destination_tab_id, ..
    } = placement_target
    {
        if *destination_tab_id != placement_snapshot.destination_tab_id {
            return;
        }
    }
    let Some(base_tab_snapshot) = placement_snapshot.find_displayed_tab_snapshot() else {
        return;
    };
    let tab_size = placement_display_tab_snapshot.tab_snapshot.tab_size;
    let pane_area = koshi_renderer::compute_pane_area(committed_regions, render_area);
    let layout_rect = koshi_renderer::compute_content_rect(pane_area, tab_size);
    let base_tab_size = base_tab_snapshot.tab_snapshot.tab_size;
    let base_layout_rect = koshi_renderer::compute_content_rect(pane_area, base_tab_size);
    let Some(target_outline) = build_target_outline_rect(
        base_tab_snapshot,
        placement_target,
        base_tab_size,
        base_layout_rect,
    ) else {
        return;
    };
    let source_pane_rect = displayed_render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .find_visible_pane_slot(placement_snapshot.source_pane_id)
        .and_then(|pane_slot| project_core_rect(pane_slot.outer_rect, tab_size, layout_rect));
    apply_placement_target_outline_style(target_outline, source_pane_rect, theme, render_buffer);
}

/// Paint every border cell of `target_outline` inside `render_buffer` in
/// `theme.accent_color`, bold. A cell that also lies on the border of
/// `source_pane_rect` keeps its style. Cell symbols never change.
fn apply_placement_target_outline_style(
    target_outline: Rect,
    source_pane_rect: Option<Rect>,
    theme: &Theme,
    render_buffer: &mut Buffer,
) {
    let visible_target_outline = target_outline.intersection(render_buffer.area);
    for row_index in visible_target_outline.top()..visible_target_outline.bottom() {
        for column_index in visible_target_outline.left()..visible_target_outline.right() {
            let is_target_outline_cell =
                is_point_on_rect_border(column_index, row_index, target_outline);
            let is_source_border_cell = source_pane_rect.is_some_and(|source_pane_rect| {
                is_point_on_rect_border(column_index, row_index, source_pane_rect)
            });
            if is_target_outline_cell && !is_source_border_cell {
                let target_border_cell = &mut render_buffer[(column_index, row_index)];
                target_border_cell.set_fg(theme.accent_color);
                target_border_cell.modifier.insert(Modifier::BOLD);
            }
        }
    }
}

/// Return whether one screen cell lies on a rectangle's border.
fn is_point_on_rect_border(column_index: u16, row_index: u16, pane_rect: Rect) -> bool {
    pane_rect.contains(Position::new(column_index, row_index))
        && (column_index == pane_rect.left()
            || column_index + 1 == pane_rect.right()
            || row_index == pane_rect.top()
            || row_index + 1 == pane_rect.bottom())
}

/// Interpolate one placement tab at `progress`, clamped to `0.0..=1.0`.
/// Progress `0.0` returns `from_tab_snapshot` and `1.0` returns
/// `to_tab_snapshot`. Two tabs solved at different tab sizes give
/// `to_tab_snapshot` unchanged.
///
/// - Every pane slot of either tab is kept, `to_tab_snapshot` order first. Its
///   rectangles move from the `from_tab_snapshot` rect to the
///   `to_tab_snapshot` rect. A pane in only one tab grows from, or shrinks to,
///   zero size at its own origin.
/// - `is_suppressed`, the stack headers, the layout mode, and
///   `is_every_pane_suppressed` come from `from_tab_snapshot` below progress
///   `0.5`, then from `to_tab_snapshot`. A pane in only one tab keeps its own
///   `is_suppressed`.
/// - The tree and the focus history come from `to_tab_snapshot`.
pub(crate) fn interpolate_placement_tab_snapshot(
    from_tab_snapshot: &PlacementTabSnapshot,
    to_tab_snapshot: &PlacementTabSnapshot,
    progress: f32,
) -> PlacementTabSnapshot {
    let progress = progress.clamp(0.0, 1.0);
    if progress == 0.0 {
        return from_tab_snapshot.clone();
    }
    if progress == 1.0
        || from_tab_snapshot.tab_snapshot.tab_size != to_tab_snapshot.tab_snapshot.tab_size
    {
        return to_tab_snapshot.clone();
    }
    let to_pane_ids = to_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect::<Vec<_>>();
    let from_only_pane_ids = from_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .filter(|pane_id| !to_pane_ids.contains(pane_id));
    let pane_slots = to_pane_ids
        .iter()
        .copied()
        .chain(from_only_pane_ids)
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
            pane_slot.outer_rect = interpolate_optional_rect(
                from_pane_slot.map(|pane_slot| pane_slot.outer_rect),
                to_pane_slot.map(|pane_slot| pane_slot.outer_rect),
                progress,
            )?;
            pane_slot.content_rect = interpolate_optional_rect(
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
    let nearer_tab_snapshot = if progress < 0.5 {
        &from_tab_snapshot.tab_snapshot
    } else {
        &to_tab_snapshot.tab_snapshot
    };
    let mut animated_tab_snapshot = to_tab_snapshot.clone();
    animated_tab_snapshot.tab_snapshot.pane_slots = pane_slots;
    animated_tab_snapshot
        .tab_snapshot
        .stack_headers
        .clone_from(&nearer_tab_snapshot.stack_headers);
    animated_tab_snapshot.tab_snapshot.layout_mode = nearer_tab_snapshot.layout_mode;
    animated_tab_snapshot.tab_snapshot.is_every_pane_suppressed =
        nearer_tab_snapshot.is_every_pane_suppressed;
    animated_tab_snapshot
}

/// Interpolate the committed tab a viewer draws while a committed pane
/// placement slides into place.
///
/// A pane with a content rect in both tabs moves from its `from_tab_snapshot`
/// rects to its `to_tab_snapshot` rects. Every other pane keeps its
/// `to_tab_snapshot` slot, and a pane only in `from_tab_snapshot` is not drawn.
/// Stack headers come from `from_tab_snapshot` while `progress` is below `0.5`,
/// then from `to_tab_snapshot`. `progress` is clamped to `0.0..=1.0`. A pane at
/// columns `0..40` in `from_tab_snapshot` and `40..80` in `to_tab_snapshot`
/// sits at columns `20..60` at progress `0.5`.
pub(crate) fn interpolate_committed_placement_tab_snapshot(
    from_tab_snapshot: &TabSnapshot,
    to_tab_snapshot: &TabSnapshot,
    progress: f32,
) -> TabSnapshot {
    let progress = progress.clamp(0.0, 1.0);
    let mut animated_tab_snapshot = to_tab_snapshot.clone();
    for pane_slot in &mut animated_tab_snapshot.pane_slots {
        let Some(to_content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(from_pane_slot) = from_tab_snapshot
            .pane_slots
            .iter()
            .find(|from_pane_slot| from_pane_slot.pane_id == pane_slot.pane_id)
        else {
            continue;
        };
        let Some(from_content_rect) = from_pane_slot.content_rect else {
            continue;
        };
        pane_slot.outer_rect =
            interpolate_rect(from_pane_slot.outer_rect, pane_slot.outer_rect, progress);
        pane_slot.content_rect = Some(interpolate_rect(
            from_content_rect,
            to_content_rect,
            progress,
        ));
    }
    if progress < 0.5 {
        animated_tab_snapshot
            .stack_headers
            .clone_from(&from_tab_snapshot.stack_headers);
    }
    animated_tab_snapshot
}

/// Interpolate one pane rectangle without overshoot.
fn interpolate_optional_rect(
    from_rect: Option<CoreRect>,
    to_rect: Option<CoreRect>,
    progress: f32,
) -> Option<CoreRect> {
    match (from_rect, to_rect) {
        (Some(from_rect), Some(to_rect)) => Some(interpolate_rect(from_rect, to_rect, progress)),
        (Some(from_rect), None) if progress < 1.0 => Some(CoreRect::from_origin_and_size(
            from_rect.origin,
            interpolate_size(
                from_rect.size,
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
                to_rect.size,
                progress,
            ),
        )),
        _ => None,
    }
}

/// Interpolate a rectangle drawn in both frames: origin and size each move
/// `progress` of the way from `from_rect` to `to_rect`.
pub(crate) fn interpolate_rect(from_rect: CoreRect, to_rect: CoreRect, progress: f32) -> CoreRect {
    CoreRect::from_origin_and_size(
        interpolate_point(from_rect.origin, to_rect.origin, progress),
        interpolate_size(from_rect.size, to_rect.size, progress),
    )
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

/// Project a core cell rectangle into one ratatui screen rectangle.
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
        core_rect.size.column_count,
        source_size.column_count,
        target_rect.x,
        target_rect.width,
    );
    let (row, row_count) = project_axis(
        core_rect.origin.row,
        core_rect.size.row_count,
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

/// Return the screen rectangle the target outline covers: `layout_rect` for a
/// whole-tab insertion, otherwise the smallest rectangle around every target
/// pane of `base_tab_snapshot` that covers cells. A target pane with an empty
/// rectangle, such as the second leaf of a collapsed stack member, adds
/// nothing. Returns `None` when no target pane covers cells.
fn build_target_outline_rect(
    base_tab_snapshot: &PlacementTabSnapshot,
    placement_target: &TiledPlacementTarget,
    tab_size: Size,
    layout_rect: Rect,
) -> Option<Rect> {
    if matches!(
        placement_target,
        TiledPlacementTarget::Split {
            anchor: PanePlacementAnchor::Tab,
            ..
        }
    ) {
        return Some(layout_rect);
    }
    base_tab_snapshot
        .tab_snapshot
        .pane_slots
        .iter()
        .filter(|pane_slot| {
            koshi_renderer::is_placement_target_pane(
                pane_slot.pane_id,
                base_tab_snapshot.tab_snapshot.tab_id,
                Some(placement_target),
            )
        })
        .filter_map(|pane_slot| project_core_rect(pane_slot.outer_rect, tab_size, layout_rect))
        .reduce(Rect::union)
}

/// The graphics capability proved by a reply from the outer terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphicsSupport {
    /// Paint image coverage with the fixed unavailable-image text.
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
        maximum_pixel_width: Option<u32>,
        /// Maximum Sixel height in pixels, or `None` when the host reports no limit.
        maximum_pixel_height: Option<u32>,
    },
}

/// A failure while painting a frame or emitting its native image data.
#[derive(Debug)]
pub(crate) enum PaintError<BackendError> {
    /// The paint driver's backend did not accept the frame's cells or cursor.
    Backend(BackendError),
    /// The native image output could not build or reset the frame's images.
    Image(io::Error),
    /// The terminal output did not take the frame's bytes.
    Output(io::Error),
}

impl GraphicsSupport {
    /// Map the terminal capability to the renderer's image mode.
    pub(crate) fn get_image_render_mode(self) -> ImageRenderMode {
        match self {
            Self::Unsupported => ImageRenderMode::Placeholder,
            Self::Kitty | Self::Iterm | Self::Sixel { .. } => ImageRenderMode::Native,
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
    const fn build_unsupported() -> Self {
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

    /// Replace the measurement for a resized viewport: `measured_cell_size`, or
    /// `None` while this attachment writes no terminal queries.
    ///
    /// A pending CSI 16t reply is marked for discard, and the call returns
    /// `false`. With no reply pending, the call returns `true` when the resized
    /// viewport has no measured cell size and a fresh request must be written.
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
    is_output_terminal: bool,
    /// Terminal handle used for protocol output and platform-mode restoration.
    terminal: Arc<Mutex<Option<TerminalDevice>>>,
    /// Parsed input source shared with the input thread. `None` when both
    /// standard streams are redirected and no terminal work is needed.
    reader: Option<InputReader>,
    /// Wakes the input reader when this attachment ends. It is present exactly
    /// when `reader` is present.
    waker: Option<PlatformWaker>,
    /// Stops input delivery before terminal restoration.
    is_shutdown_requested: Arc<AtomicBool>,
    /// Whether this attachment enabled application-level terminal modes.
    has_active_application_modes: Arc<AtomicBool>,
    /// `true` once one cleanup has written the Kitty abort and delete-all for
    /// this attachment. Every cleanup after that one skips both.
    is_image_cleanup_claimed: Arc<AtomicBool>,
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
        let is_input_terminal = io::stdin().is_terminal();
        let is_output_terminal = io::stdout().is_terminal();
        let (graphics_support, initial_cell_size, terminal, reader, waker) =
            if needs_terminal_device(is_input_terminal, is_output_terminal) {
                let (mut terminal, event_source) = TerminalDevice::open_terminal_device()
                    .map_err(|open_error| format!("could not open the terminal: {open_error}"))?;
                let mut reader = InputReader::from_event_source(event_source);
                let terminal_probe = if supports_native_images {
                    resolve_graphics_support_for_output(is_output_terminal, || {
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
                    TerminalProbe::build_unsupported()
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
        let is_image_cleanup_claimed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            graphics_support,
            initial_cell_size,
            is_output_terminal,
            terminal: Arc::new(Mutex::new(terminal)),
            reader,
            waker,
            is_shutdown_requested: Arc::new(AtomicBool::new(false)),
            has_active_application_modes: Arc::new(AtomicBool::new(false)),
            is_image_cleanup_claimed,
            input_thread: None,
            is_activated: false,
        })
    }

    /// Return the native graphics support proved by the terminal probe.
    pub(crate) fn get_graphics_support(&self) -> GraphicsSupport {
        self.graphics_support
    }

    /// Build the cell-size coordinator used after this terminal attaches.
    pub(crate) fn build_cell_size_query(&self) -> CellSizeQuery {
        CellSizeQuery::from_current_measurement(
            self.initial_cell_size,
            self.is_output_terminal
                && !matches!(self.graphics_support, GraphicsSupport::Unsupported),
            false,
        )
    }

    /// Register panic-safe image cleanup and terminal restoration.
    pub(crate) fn register_terminal_restore(&self, terminal_cleanup_guard: &TerminalCleanupGuard) {
        let terminal = Arc::clone(&self.terminal);
        let graphics_support = self.graphics_support;
        let is_shutdown_requested = Arc::clone(&self.is_shutdown_requested);
        let waker = self.waker.clone();
        let has_active_application_modes = Arc::clone(&self.has_active_application_modes);
        let is_image_cleanup_claimed = Arc::clone(&self.is_image_cleanup_claimed);
        terminal_cleanup_guard.register_cleanup(Box::new(move || {
            is_shutdown_requested.store(true, Ordering::Release);
            if let Some(waker) = &waker {
                let _ = waker.wake();
            }
            try_restore_shared_terminal(
                &terminal,
                graphics_support,
                &has_active_application_modes,
                &is_image_cleanup_claimed,
            );
        }));
    }

    /// Enable terminal modes and start input delivery after Attach succeeds.
    ///
    /// The input thread sends the events of each read of the terminal on
    /// `terminal_input_sender` as one [`TerminalInput`], stamped on
    /// `input_read_clock`.
    pub(crate) fn activate_terminal(
        &mut self,
        terminal_input_sender: mpsc::SyncSender<TerminalInput>,
        client_id: ClientId,
        input_read_clock: Arc<InputReadClock>,
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
                if self.is_output_terminal {
                    self.has_active_application_modes
                        .store(true, Ordering::Release);
                    enable_terminal_modes(terminal, self.graphics_support).map_err(
                        |mode_enable_error| {
                            format!("could not enable terminal modes: {mode_enable_error}")
                        },
                    )?;
                }
            } else if self.is_output_terminal || should_read_input {
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
        let is_shutdown_requested = Arc::clone(&self.is_shutdown_requested);
        let panic_input_sender = terminal_input_sender.clone();
        self.input_thread = Some(
            thread::Builder::new()
                .name("koshi-terminal-input".to_string())
                .spawn(move || {
                    let input_thread_result = catch_unwind(AssertUnwindSafe(|| {
                        run_terminal_input(
                            &mut input_reader,
                            &terminal_input_sender,
                            client_id,
                            &input_read_clock,
                            &is_shutdown_requested,
                        );
                    }));
                    if input_thread_result.is_err() {
                        let _ = panic_input_sender.send(TerminalInput {
                            runtime_events: vec![RuntimeEvent::Quit],
                            read_at: input_read_clock.stamp_input_read(Instant::now()),
                        });
                    }
                })
                .map_err(|thread_spawn_error| {
                    format!("could not spawn the terminal input thread: {thread_spawn_error}")
                })?,
        );
        Ok(())
    }

    /// Stop input delivery and restore the host terminal state.
    pub(crate) fn shutdown_terminal(mut self) {
        self.stop_terminal_owner();
    }

    /// Signal and join the input thread, then restore every terminal mode.
    fn stop_terminal_owner(&mut self) {
        self.is_shutdown_requested.store(true, Ordering::Release);
        if let Some(waker) = &self.waker {
            let _ = waker.wake();
        }
        if let Some(input_thread_handle) = self.input_thread.take() {
            let _ = input_thread_handle.join();
        }
        restore_shared_terminal(
            &self.terminal,
            self.graphics_support,
            &self.has_active_application_modes,
            &self.is_image_cleanup_claimed,
        );
    }
}

impl Drop for TerminalOwner {
    fn drop(&mut self) {
        self.stop_terminal_owner();
    }
}

/// Whether input or output needs access to the controlling terminal.
fn needs_terminal_device(is_input_terminal: bool, is_output_terminal: bool) -> bool {
    is_input_terminal || is_output_terminal
}

/// Probe only when standard output is the terminal that will receive images.
fn resolve_graphics_support_for_output(
    is_output_terminal: bool,
    run_terminal_probe: impl FnOnce() -> io::Result<TerminalProbe>,
) -> io::Result<TerminalProbe> {
    if is_output_terminal {
        run_terminal_probe()
    } else {
        Ok(TerminalProbe::build_unsupported())
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
        .unwrap_or_else(|poison_error| poison_error.into_inner())
}

/// Restore the shared terminal once, waiting for an in-progress terminal write.
fn restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    has_active_application_modes: &AtomicBool,
    is_image_cleanup_claimed: &AtomicBool,
) {
    let terminal_device = lock_terminal(shared_terminal).take();
    let Some(mut terminal) = terminal_device else {
        return;
    };
    restore_terminal(
        &mut terminal,
        graphics_support,
        has_active_application_modes,
        is_image_cleanup_claimed,
    );
}

/// Restore without blocking a panic hook on the thread that holds the terminal.
fn try_restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    has_active_application_modes: &AtomicBool,
    is_image_cleanup_claimed: &AtomicBool,
) {
    let mut terminal_guard = match shared_terminal.try_lock() {
        Ok(terminal_guard) => terminal_guard,
        Err(TryLockError::Poisoned(poison_error)) => poison_error.into_inner(),
        Err(TryLockError::WouldBlock) => {
            write_fallback_terminal_cleanup(
                graphics_support,
                has_active_application_modes,
                is_image_cleanup_claimed,
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
        has_active_application_modes,
        is_image_cleanup_claimed,
    );
}

/// Restore application modes and the platform's cooked terminal mode.
fn restore_terminal(
    terminal: &mut TerminalDevice,
    graphics_support: GraphicsSupport,
    has_active_application_modes: &AtomicBool,
    is_image_cleanup_claimed: &AtomicBool,
) {
    if let Err(application_restore_error) = restore_application_modes(
        terminal,
        graphics_support,
        has_active_application_modes,
        is_image_cleanup_claimed,
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
    has_active_application_modes: &AtomicBool,
    is_image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    if !has_active_application_modes.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    write_terminal_cleanup(writer, graphics_support, is_image_cleanup_claimed)
}

/// Claim the one Kitty cleanup allowed for an attachment.
fn claim_image_cleanup(is_image_cleanup_claimed: &AtomicBool) -> bool {
    !is_image_cleanup_claimed.swap(true, Ordering::AcqRel)
}

/// Gather the capability replies up to the reply to the primary device
/// attributes query, which the probe asks last, or up to one shared deadline
/// when that reply never comes. Example: a terminal that answers the Kitty
/// query and then the device attributes query ends the probe at its second
/// reply.
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
        let is_last_probe_reply = matches!(probe_event, Event::PrimaryDeviceAttributes(_));
        probe_replies.record_probe_event(probe_event);
        if is_last_probe_reply || Instant::now() >= probe_deadline {
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
    fn record_probe_event(&mut self, probe_event: Event) {
        match probe_event {
            Event::KittyGraphicsReply(kitty_reply)
                if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID =>
            {
                self.supports_kitty |= kitty_reply.is_successful;
            }
            Event::TerminalFeatures(terminal_features) => {
                self.supports_iterm_file_output |= supports_iterm_file_feature(&terminal_features);
                self.supports_iterm_sixel |= supports_iterm_sixel_feature(&terminal_features);
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

    /// Return Sixel support when the DA1 reply lists attribute `4`, the iTerm2
    /// feature reply lists Sixel, or the geometry reply succeeded; otherwise
    /// `None`. A palette reply below `2` colors returns `None`. The palette is
    /// capped at `256` colors and is `2` when the palette reply failed or never
    /// came. A geometry width or height of `0` means no limit.
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
        let (maximum_pixel_width, maximum_pixel_height) = match self.sixel_geometry {
            Some(Ok((pixel_width, pixel_height))) => (
                (pixel_width != 0).then_some(pixel_width),
                (pixel_height != 0).then_some(pixel_height),
            ),
            Some(Err(_)) | None => (None, None),
        };
        Some(GraphicsSupport::Sixel {
            palette_color_count,
            maximum_pixel_width,
            maximum_pixel_height,
        })
    }
}

/// Write every bounded capability query and flush them as one probe batch.
fn write_terminal_probe_queries<W: Write>(writer: &mut W) -> io::Result<()> {
    write_kitty_support_query(writer).map_err(convert_kitty_output_error)?;
    writer.write_all(ITERM_CAPABILITIES_QUERY)?;
    writer.write_all(SIXEL_PALETTE_QUERY)?;
    writer.write_all(SIXEL_GEOMETRY_QUERY)?;
    writer.write_all(CELL_SIZE_QUERY_BYTES)?;
    writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    writer.flush()
}

/// Return whether an event belongs to the capability probe.
fn is_probe_event(probe_event: &Event) -> bool {
    matches!(
        probe_event,
        Event::KittyGraphicsReply(kitty_reply) if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID
    ) || matches!(
        probe_event,
        Event::TerminalFeatures(_)
            | Event::PrimaryDeviceAttributes(_)
            | Event::SixelGraphicsAttributeReply(_)
            | Event::CellSize(_)
    )
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

/// Request Shift mouse reports while pane placement owns the mouse gesture.
/// The outer terminal may ignore this request. A successful write and flush
/// records the requested mode. A call whose mode equals the recorded mode
/// writes nothing.
pub(crate) fn update_shift_mouse_capture<W: Write>(
    writer: &mut W,
    should_request_shift_mouse_capture: bool,
    is_shift_mouse_capture_requested: &mut bool,
) -> io::Result<()> {
    if *is_shift_mouse_capture_requested == should_request_shift_mouse_capture {
        return Ok(());
    }
    let shift_mouse_capture_bytes = if should_request_shift_mouse_capture {
        SHIFT_MOUSE_CAPTURE_REQUEST_BYTES
    } else {
        SHIFT_MOUSE_CAPTURE_RELEASE_BYTES
    };
    writer.write_all(shift_mouse_capture_bytes)?;
    writer.flush()?;
    *is_shift_mouse_capture_requested = should_request_shift_mouse_capture;
    Ok(())
}

/// Write every application-level terminal reset in reverse setup order.
fn write_terminal_cleanup<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
    is_image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    let mut first_io_error = None;
    if matches!(graphics_support, GraphicsSupport::Kitty)
        && claim_image_cleanup(is_image_cleanup_claimed)
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
    has_active_application_modes: &AtomicBool,
    is_image_cleanup_claimed: &AtomicBool,
) {
    let mut stdout = io::stdout();
    let _ = restore_application_modes(
        &mut stdout,
        graphics_support,
        has_active_application_modes,
        is_image_cleanup_claimed,
    );
}

/// Read semantic terminal events until shutdown or input failure, and send
/// the events of each read of the terminal on `terminal_input_sender` as one
/// [`TerminalInput`], with the stamp `input_read_clock` gives that read. A read
/// whose events all convert to nothing sends nothing. An input failure sends
/// a [`TerminalInput`] that holds [`RuntimeEvent::Quit`] alone, and ends.
fn run_terminal_input<EventSourceType: reader::EventSource>(
    input_reader: &mut InputReader<EventSourceType>,
    terminal_input_sender: &mpsc::SyncSender<TerminalInput>,
    client_id: ClientId,
    input_read_clock: &InputReadClock,
    is_shutdown_requested: &AtomicBool,
) {
    while !is_shutdown_requested.load(Ordering::Acquire) {
        let runtime_events: Vec<RuntimeEvent> =
            match input_reader.read_events_of_next_terminal_read() {
                Ok(host_events) => host_events
                    .into_iter()
                    .filter_map(|host_event| build_terminal_runtime_event(client_id, host_event))
                    .collect(),
                Err(input_read_error)
                    if input_read_error.kind() == io::ErrorKind::Interrupted
                        && is_shutdown_requested.load(Ordering::Acquire) =>
                {
                    break;
                }
                Err(input_read_error) if input_read_error.kind() == io::ErrorKind::Interrupted => {
                    continue
                }
                Err(input_read_error) => {
                    tracing::warn!(%input_read_error, "could not read terminal input");
                    vec![RuntimeEvent::Quit]
                }
            };
        if runtime_events.is_empty() {
            continue;
        }
        let is_hangup = matches!(runtime_events.as_slice(), [RuntimeEvent::Quit]);
        let terminal_input = TerminalInput {
            runtime_events,
            read_at: input_read_clock.stamp_input_read(Instant::now()),
        };
        if terminal_input_sender.send(terminal_input).is_err() || is_hangup {
            break;
        }
    }
}

/// The events of one read of this terminal and the instant it was read: the
/// events its input thread parsed from that read, or this terminal hanging up
/// as a read failure, a termination signal, or a console close reports it.
#[derive(Debug)]
pub(crate) struct TerminalInput {
    /// The events, oldest first: keys, mouse events, a paste, a resize, a cell
    /// size reply, and focus losses. A hangup is [`RuntimeEvent::Quit`] alone.
    /// Typing `é-` that the terminal sends in one read is the two keys `é`
    /// and `-`.
    pub(crate) runtime_events: Vec<RuntimeEvent>,
    /// When the input thread read the events, as
    /// [`InputReadClock::stamp_input_read`] stamped that read, or when a
    /// hangup report arrived. The attach loop runs every input timer due at
    /// this instant before it handles the events, and ends at a hangup.
    pub(crate) read_at: Instant,
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

/// `window_size` cut to at most
/// [`MAX_VIEWPORT_CELL_COUNT`](koshi_core::constant::MAX_VIEWPORT_CELL_COUNT) cells, as
/// [`limit_viewport_size`] cuts it. A cut size carries no pixel dimensions.
/// `65535x65535` cells at `4096x4096` pixels → `65535x32` cells and no
/// pixels; `200x50` cells keep their pixels.
pub(crate) fn limit_window_size(window_size: WindowSize) -> WindowSize {
    let viewport_size = Size {
        column_count: window_size.column_count,
        row_count: window_size.row_count,
    };
    let limited_viewport_size = limit_viewport_size(viewport_size);
    if limited_viewport_size == viewport_size {
        return window_size;
    }
    WindowSize {
        column_count: limited_viewport_size.column_count,
        row_count: limited_viewport_size.row_count,
        pixel_width: None,
        pixel_height: None,
    }
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

/// Build the viewer half and apply `loaded_config`'s viewer-owned files, in
/// one step.
///
/// `client_id` is the id this viewer's input events and commands carry.
/// `viewport_size` is this terminal's size in cells. `terminal_cleanup_guard`
/// is the guard that restores the outer terminal. `input_read_clock` is the
/// clock this terminal's input thread stamps each input event on.
///
/// `loaded_config.app_config_layer` and `loaded_config.theme_config_layer`
/// fold into the viewer's settings and chrome colors and always apply.
/// `loaded_config.keybindings_config_layer` is validated: a verdict other than
/// [`Apply`](koshi_config::conflict::KeymapVerdict::Apply) logs a warning
/// naming `koshi keys conflicts`, an `Apply` logs `"keybinding.kdl applied"`,
/// and a `None` keymap layer logs nothing.
pub(crate) fn build_client_with_loaded_config(
    client_id: ClientId,
    viewport_size: Size,
    terminal_cleanup_guard: TerminalCleanupGuard,
    loaded_config: koshi_link::config::LoadedConfig,
    input_read_clock: Arc<InputReadClock>,
) -> Client {
    let mut viewer_client =
        Client::from_client_id_and_viewport_size(client_id, viewport_size, terminal_cleanup_guard);
    viewer_client.input_read_clock = input_read_clock;
    match viewer_client.load_startup_config(
        loaded_config.app_config_layer,
        loaded_config.theme_config_layer,
        loaded_config.keybindings_config_layer,
    ) {
        Some(conflict_report)
            if conflict_report.get_verdict() != koshi_config::conflict::KeymapVerdict::Apply =>
        {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }
    viewer_client
}

/// The inputs one frame of [`paint_frame_with_writer`] draws its cells from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameInputs {
    /// The frame in the pane layout shown to the user.
    render_snapshot: RenderSnapshot,
    /// The region solve the frame is drawn with.
    committed_regions: CommittedRegions,
    /// The colors of koshi's chrome.
    theme: Theme,
    /// The hint bar's data.
    keymap_hints: KeymapHints,
    /// The multi-chord sequence the viewer has open.
    pending_key_sequence: Option<KeySequence>,
    /// The hovered pane, the tab strip's position and the reconnect state.
    viewer_chrome: ViewerChrome,
    /// The image mode of the outer terminal.
    image_mode: ImageRenderMode,
    /// The image placements that show the unavailable-image text with native
    /// image output.
    unavailable_image_placement_keys: Vec<ImagePlacementKey>,
    /// The frame's size.
    terminal_size: Size,
    /// Whether the frame draws a placement preview, outline or explanation.
    has_placement_state: bool,
}

impl FrameInputs {
    /// The rows in which the cells of a frame drawn from `next_frame_inputs`
    /// can differ from those of a frame drawn from these inputs, as
    /// [`list_changed_screen_rows`] gives them. `None` when any input other
    /// than the render snapshot differs, when either frame draws a placement
    /// state, or when the render snapshots differ in anything other than pane
    /// rows.
    fn list_changed_screen_rows(&self, next_frame_inputs: &FrameInputs) -> Option<Vec<u16>> {
        let FrameInputs {
            render_snapshot,
            committed_regions,
            theme,
            keymap_hints,
            pending_key_sequence,
            viewer_chrome,
            image_mode,
            unavailable_image_placement_keys,
            terminal_size,
            has_placement_state,
        } = next_frame_inputs;
        if self.has_placement_state
            || *has_placement_state
            || self.committed_regions != *committed_regions
            || self.theme != *theme
            || self.keymap_hints != *keymap_hints
            || self.pending_key_sequence != *pending_key_sequence
            || self.viewer_chrome != *viewer_chrome
            || self.image_mode != *image_mode
            || self.unavailable_image_placement_keys != *unavailable_image_placement_keys
            || self.terminal_size != *terminal_size
        {
            return None;
        }
        list_changed_screen_rows(
            &self.render_snapshot,
            render_snapshot,
            committed_regions,
            *viewer_chrome,
            Rect::new(0, 0, terminal_size.column_count, terminal_size.row_count),
        )
    }
}

/// Draw `displayed_snapshot`, the frame in the pane layout shown to the user,
/// through `paint_driver`, keeping the outer terminal's window title and
/// cursor style in step with the focused pane.
///
/// The frame is drawn at `client`'s viewport size, the size of the last resize
/// event: no terminal size is read. The theme comes from `client`, and so does
/// the hint bar, built for `frame_paint.lock_mode`,
/// `frame_paint.is_mouse_selection_enabled`, and whether `displayed_snapshot`
/// focuses a floating pane
/// ([`KeymapHintCatalog::build_frame_hints`](koshi_config::hints::KeymapHintCatalog::build_frame_hints)).
/// The hovered pane, the tab strip's position and the open key sequence come
/// from `frame_paint`.
/// `committed_regions` is the geometry shared by the painter and cursor
/// placement for this frame.
///
/// Every byte of the frame goes to `writer`, and the backend of
/// `paint_driver` writes into the same held bytes, in this order: the
/// synchronized-update start and the image reset of a native image frame, the
/// changed cells and the cursor, the window title, the cursor style, the
/// native image bytes, and the synchronized-update end. One flush of `writer`
/// at the end writes the frame out.
///
/// `shown_frame_inputs` holds the inputs of the last frame drawn. A frame
/// whose inputs differ from them only in pane rows draws and compares the
/// changed rows alone ([`PaintDriver::draw_frame`]). A drawn frame stores its
/// own inputs there, and a failed frame stores `None`.
///
/// `last_window_title` and `last_cursor_style` store the title and cursor style used to
/// decide whether the next frame needs a control write. Both are read before
/// the buffer paint and updated after it succeeds: a changed title writes
/// `SetTitle`, and a changed cursor style writes `SetCursorStyle`. A frame
/// that [`get_cursor_style`] names no style stores `None` and writes no style
/// command.
///
/// With native image output, the frame's text never waits for an image
/// encode. Each image shows the pixels [`ImageOutputState::prepare_frame`]
/// picks for it, and the frame writes native image bytes only when those
/// pixels differ from the ones the outer terminal shows. Example: a key typed
/// into one pane while a Sixel image in another pane encodes draws at once,
/// and the image shows its older pixels, or none, until its encode ends.
///
/// A frame that clears the whole screen before its cells
/// ([`PaintDriver::needs_screen_erase`]), such as the first frame after a
/// host resize, records that the outer terminal shows no image
/// ([`ImageOutputState::clear_host_images`]) before it picks the pixels. It
/// then writes every image it has pixels for after its cells. Example: a
/// Sixel image at `(0, 0)` and a resize from 80×24 to 100×30 give one frame:
/// `ESC[2J`, the cells, then the Sixel image again from its kept encode.
///
/// # Errors
///
/// Returns a backend, output or native-image error when the frame cannot be
/// fully painted. After an error that follows the first written byte, the
/// shown screen is unknown ([`PaintDriver::mark_shown_screen_unknown`]), and
/// with native image output the image commit fails
/// ([`ImageOutputState::fail_frame_commit`]): the next frame clears the screen
/// and draws every cell and image again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frame_with_writer<B: Backend, W: Write>(
    writer: &mut W,
    paint_driver: &mut PaintDriver<B>,
    client: &Client,
    displayed_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    shown_frame_inputs: &mut Option<FrameInputs>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
    placement_snapshot: Option<&PlacementSnapshot>,
    placement_display_tab_snapshot: Option<&PlacementTabSnapshot>,
) -> Result<(), PaintError<B::Error>> {
    let window_title_text = build_window_title(displayed_snapshot);
    let is_window_title_changed = window_title_text != *last_window_title;
    let cursor_style = get_cursor_style(displayed_snapshot);
    let is_cursor_style_changed = cursor_style != *last_cursor_style;
    let keymap_hints = client.keymap_catalog.build_frame_hints(
        frame_paint.lock_mode,
        frame_paint.is_mouse_selection_enabled,
        is_focused_pane_floating(
            &displayed_snapshot.session_snapshot,
            &displayed_snapshot.client_snapshot,
        ),
    );
    let terminal_size = client.get_viewport_size();
    let render_area = Rect::new(0, 0, terminal_size.column_count, terminal_size.row_count);
    let is_focused_pane_softened = displayed_snapshot
        .client_snapshot
        .focused_pane_id
        .is_some_and(|focused_pane_id| {
            frame_paint
                .placement_presentation
                .as_ref()
                .is_some_and(|placement_presentation| {
                    placement_presentation.is_pane_affected(focused_pane_id)
                })
        });
    let hardware_cursor_position = get_cursor_position(
        displayed_snapshot,
        committed_regions,
        frame_paint.chrome,
        render_area,
    )
    .filter(|_| !is_focused_pane_softened);
    let is_native_image_output = image_output_state.get_output_kind().is_some();
    if paint_driver.needs_screen_erase(terminal_size) {
        image_output_state.clear_host_images();
    }
    let mut image_paint_commands = build_image_paints(
        displayed_snapshot,
        committed_regions,
        frame_paint.chrome,
        render_area,
    );
    if let Some(placement_presentation) = &frame_paint.placement_presentation {
        image_paint_commands
            .retain(|image_paint| !placement_presentation.is_pane_affected(image_paint.pane_id));
    }
    let image_target_bounds = image_paint_commands
        .iter()
        .map(|image_paint| image_paint.target_area)
        .reduce(Rect::union);
    let image_cell_composition_snapshot =
        match (image_output_state.get_output_kind(), image_target_bounds) {
            (Some(output_kind), Some(image_target_bounds))
                if output_kind.needs_cell_composition() =>
            {
                build_image_cell_snapshot(
                    displayed_snapshot,
                    committed_regions,
                    frame_paint.chrome,
                    render_area,
                    image_target_bounds,
                )
                .map(Arc::new)
            }
            _ => None,
        };
    if is_native_image_output {
        image_output_state.prepare_frame(
            &image_paint_commands,
            image_cell_composition_snapshot,
            cell_size,
        );
    }
    let is_native_image_commit =
        is_native_image_output && image_output_state.is_native_commit_pending();
    let native_image_output_bytes = if is_native_image_commit {
        match image_output_state.build_frame_output(hardware_cursor_position) {
            Ok(native_image_output_bytes) => native_image_output_bytes,
            Err(image_frame_error) => {
                image_output_state.fail_frame_commit();
                return Err(PaintError::Image(image_frame_error));
            }
        }
    } else {
        Vec::new()
    };
    let frame_inputs = FrameInputs {
        render_snapshot: displayed_snapshot.clone(),
        committed_regions: committed_regions.clone(),
        theme: *client.get_theme(),
        keymap_hints,
        pending_key_sequence: frame_paint.pending_key_sequence.clone(),
        viewer_chrome: frame_paint.chrome,
        image_mode,
        unavailable_image_placement_keys: image_output_state
            .list_unavailable_placement_keys()
            .to_vec(),
        terminal_size,
        has_placement_state: placement_snapshot.is_some()
            || placement_display_tab_snapshot.is_some()
            || frame_paint.placement_target.is_some()
            || frame_paint.placement_presentation.is_some(),
    };
    let changed_rows = shown_frame_inputs
        .take()
        .and_then(|last_frame_inputs| last_frame_inputs.list_changed_screen_rows(&frame_inputs));
    let paint_result = (|| {
        if is_native_image_commit {
            queue!(writer, BeginSynchronizedUpdate).map_err(PaintError::Output)?;
        }
        if is_native_image_output
            && image_output_state
                .write_frame_reset(writer)
                .map_err(PaintError::Image)?
        {
            paint_driver.clear_shown_cells();
        }
        paint_driver
            .draw_frame(
                terminal_size,
                changed_rows.as_deref(),
                |frame_buffer, drawn_rows| {
                    SnapshotWidget {
                        render_snapshot: &frame_inputs.render_snapshot,
                        theme: &frame_inputs.theme,
                        keymap_hints: &frame_inputs.keymap_hints,
                        pending_key_sequence: frame_inputs.pending_key_sequence.as_ref(),
                        viewer_chrome: frame_inputs.viewer_chrome,
                        committed_regions: &frame_inputs.committed_regions,
                        image_mode,
                        unavailable_image_placement_keys: &frame_inputs
                            .unavailable_image_placement_keys,
                        placement_snapshot,
                        placement_display_tab_snapshot,
                        placement_target: frame_paint.placement_target.as_ref(),
                        placement_presentation: frame_paint.placement_presentation.as_ref(),
                        drawn_rows,
                    }
                    .render(render_area, frame_buffer);
                    hardware_cursor_position
                },
            )
            .map_err(PaintError::Backend)?;
        if is_window_title_changed {
            queue!(writer, SetTitle(&window_title_text)).map_err(PaintError::Output)?;
        }
        if is_cursor_style_changed {
            if let Some(cursor_command) = cursor_style.map(resolve_cursor_style_command) {
                queue!(writer, cursor_command).map_err(PaintError::Output)?;
            }
        }
        if is_native_image_commit {
            writer
                .write_all(&native_image_output_bytes)
                .map_err(PaintError::Output)?;
            queue!(writer, EndSynchronizedUpdate).map_err(PaintError::Output)?;
        }
        writer.flush().map_err(PaintError::Output)
    })();
    if let Err(frame_paint_error) = paint_result {
        paint_driver.mark_shown_screen_unknown();
        if is_native_image_output {
            image_output_state.fail_frame_commit();
        }
        if is_native_image_commit {
            recover_synchronized_frame(writer);
        } else {
            let _ = writer.flush();
        }
        return Err(frame_paint_error);
    }
    if is_native_image_commit {
        image_output_state.commit_frame();
    }
    *shown_frame_inputs = Some(frame_inputs);
    if is_window_title_changed {
        *last_window_title = window_title_text;
    }
    if is_cursor_style_changed {
        *last_cursor_style = cursor_style;
    }
    Ok(())
}

/// Cancel an incomplete terminal control string and close synchronized output.
fn recover_synchronized_frame<W: Write>(writer: &mut W) {
    let _ = image_output::write_image_abort(writer);
    let _ = queue!(writer, EndSynchronizedUpdate);
    let _ = writer.flush();
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
pub(crate) fn resolve_cursor_style_command(cursor_style: CursorStyle) -> SetCursorStyle {
    let CursorStyle::Shaped { shape, is_blinking } = cursor_style else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (shape, is_blinking) {
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
        .and_then(|focused_pane_id| render_snapshot.find_pane_snapshot(focused_pane_id))
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
