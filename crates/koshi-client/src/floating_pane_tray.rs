//! The viewer's floating pane tray: opening and closing it, the tile its keys
//! act on, and the command a key or a click on it sends.
//!
//! The tray is this viewer's alone. Opening, closing and selecting send
//! nothing. A tile toggle sends `SetPaneMinimized` for the tile's pane, and the
//! header label or `core:toggle-minimize-all-floating-panes` sends
//! `SetAllFloatingPanesMinimized`. Each changes the view of this client only.

use koshi_core::command::{Command, SetAllFloatingPanesMinimizedArgs, SetPaneMinimizedArgs};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{CommandId, PaneId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_renderer::floating_pane_tray::{
    compute_floating_pane_tray_layout, find_floating_pane_tray_glyph_cell, FloatingPaneTrayLayout,
};
use koshi_renderer::snapshot::{
    CommittedRegions, FloatingPane, FloatingPaneTray, FrameLayout, LinkStatus, MouseFrame,
    RenderSnapshot, SessionSnapshot, ViewerChrome,
};
use koshi_renderer::{resolve_hit_region, HitRegion};

use crate::mouse::MouseAction;
use crate::{Client, PlacementInputAction};

#[cfg(test)]
mod tests;

impl Client {
    /// Close the tray when it is open, and open it
    /// ([`open_floating_pane_tray`](Self::open_floating_pane_tray)) when it is
    /// closed. Sends nothing.
    pub(crate) fn toggle_floating_pane_tray(&mut self, mouse_frame: Option<&MouseFrame>) {
        if self.floating_pane_tray.is_some() {
            self.close_floating_pane_tray();
        } else {
            self.open_floating_pane_tray(mouse_frame);
        }
    }

    /// Open the tray over `mouse_frame`, the frame this viewer last painted,
    /// with the tile of the focused floating pane selected, else tile `0`. It
    /// drops an open key sequence and every gesture a held button drives
    /// ([`end_held_mouse_gestures`](Self::end_held_mouse_gestures)).
    ///
    /// Opens nothing while another local submode is on, while the viewer is
    /// not [`LinkStatus::Linked`], with no painted frame, and when the frame
    /// has no room for the tray ([`compute_floating_pane_tray_layout`]): the
    /// session holds no floating pane, or the pane area is too small.
    ///
    /// With floating panes `htop`, `logs` and `notes` and `logs` focused, the
    /// tray opens with tile `1` selected.
    pub(crate) fn open_floating_pane_tray(&mut self, mouse_frame: Option<&MouseFrame>) {
        if self.find_viewer_local_input_mode().is_some() || self.link_status != LinkStatus::Linked {
            return;
        }
        let Some(mouse_frame) = mouse_frame else {
            return;
        };
        let selected_tile_index = mouse_frame
            .client_snapshot
            .focused_pane_id
            .and_then(|focused_pane_id| {
                mouse_frame
                    .session_snapshot
                    .find_floating_pane_index(focused_pane_id)
            })
            .unwrap_or(0);
        let floating_pane_tray = FloatingPaneTray {
            selected_tile_index,
            hovered_tile_index: None,
        };
        if find_floating_pane_tray_layout(mouse_frame, floating_pane_tray).is_none() {
            return;
        }
        self.end_held_mouse_gestures();
        self.pending_key_sequence = None;
        self.floating_pane_tray = Some(floating_pane_tray);
    }

    /// Close the tray and drop an open key sequence. Sends nothing. Does
    /// nothing while the tray is closed.
    pub(crate) fn close_floating_pane_tray(&mut self) {
        if self.floating_pane_tray.take().is_some() {
            self.pending_key_sequence = None;
        }
    }

    /// Select the tile one step in `direction` from the selected tile, in the
    /// tray's grid as `mouse_frame` lays it out: `Left` and `Right` step in the
    /// row, `Up` and `Down` step one row. A step past the grid's edge, or onto
    /// a slot that shows no floating pane, selects nothing new. Does nothing
    /// while the tray is closed or with no painted frame.
    ///
    /// In a tray of six slots per row with eight floating panes, `Down` from
    /// tile `1` selects tile `7`, and `Down` from tile `2` stays on tile `2`.
    pub(crate) fn select_tray_tile(
        &mut self,
        direction: Direction,
        mouse_frame: Option<&MouseFrame>,
    ) {
        let (Some(floating_pane_tray), Some(mouse_frame)) = (self.floating_pane_tray, mouse_frame)
        else {
            return;
        };
        let Some(tray_layout) = find_floating_pane_tray_layout(mouse_frame, floating_pane_tray)
        else {
            return;
        };
        let slots_per_row = usize::from(tray_layout.slots_per_row);
        let tile_count = mouse_frame.session_snapshot.floating_panes.len();
        let selected_tile_index = floating_pane_tray.selected_tile_index;
        let column_index = selected_tile_index % slots_per_row;
        let next_tile_index = match direction {
            Direction::Left if column_index > 0 => Some(selected_tile_index - 1),
            Direction::Right if column_index + 1 < slots_per_row => Some(selected_tile_index + 1),
            Direction::Up => selected_tile_index.checked_sub(slots_per_row),
            Direction::Down => Some(selected_tile_index + slots_per_row),
            Direction::Left | Direction::Right => None,
        };
        if let Some(next_tile_index) = next_tile_index.filter(|&tile_index| tile_index < tile_count)
        {
            self.floating_pane_tray = Some(FloatingPaneTray {
                selected_tile_index: next_tile_index,
                ..floating_pane_tray
            });
        }
    }

    /// The `SetPaneMinimized` that toggles the selected tile's pane
    /// ([`build_tray_tile_toggle_command`]), sent as a key binding's command.
    /// `Consumed` while the tray is closed, with no painted frame, or when the
    /// selected tile shows no pane.
    pub(crate) fn toggle_selected_tray_tile(
        &self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let (Some(floating_pane_tray), Some(mouse_frame)) = (self.floating_pane_tray, mouse_frame)
        else {
            return PlacementInputAction::Consumed;
        };
        let Some(floating_pane) = mouse_frame
            .session_snapshot
            .floating_panes
            .get(floating_pane_tray.selected_tile_index)
        else {
            return PlacementInputAction::Consumed;
        };
        PlacementInputAction::SubmitCommand {
            command_id: CommandId::new(),
            command: build_tray_tile_toggle_command(floating_pane),
        }
    }

    /// Keep the open tray in step with `render_snapshot`, the frame just
    /// applied, painted with `committed_regions`: a selected tile index past
    /// the last floating pane moves to the last one, and a hovered tile index
    /// past it is dropped. The tray closes
    /// ([`close_floating_pane_tray`](Self::close_floating_pane_tray)) when
    /// the frame has no room for it ([`compute_floating_pane_tray_layout`]):
    /// the session holds no floating pane, every pane is suppressed, or the
    /// pane area is too small. Does nothing while the tray is closed.
    ///
    /// With the tray open on tile `2` of three, a frame with two floating
    /// panes selects tile `1`, and a frame with none closes the tray.
    pub(crate) fn update_floating_pane_tray(
        &mut self,
        render_snapshot: &RenderSnapshot,
        committed_regions: &CommittedRegions,
    ) {
        let Some(floating_pane_tray) = self.floating_pane_tray else {
            return;
        };
        let floating_pane_count = render_snapshot.session_snapshot.floating_panes.len();
        self.floating_pane_tray = Some(FloatingPaneTray {
            selected_tile_index: floating_pane_tray
                .selected_tile_index
                .min(floating_pane_count.saturating_sub(1)),
            hovered_tile_index: floating_pane_tray
                .hovered_tile_index
                .filter(|&hovered_tile_index| hovered_tile_index < floating_pane_count),
        });
        let viewer_chrome = self.build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id);
        let frame_layout =
            render_snapshot.build_painted_frame_layout(viewer_chrome, committed_regions);
        if compute_floating_pane_tray_layout(frame_layout).is_none() {
            self.close_floating_pane_tray();
        }
    }

    /// The box the floating pane `pane_id` of `render_snapshot` minimizes into
    /// and restores out of, counted from the pane-area origin of
    /// `committed_regions`: the pane's tile while this viewer's tray is open,
    /// else the pane-area cell under the tray button's glyph `⧉`. `None` while
    /// the tray is open and the frame has no room for it or no tile for the
    /// pane, and while the frame draws no tray button.
    ///
    /// Over a pane area at `(0, 1)`, a closed tray whose glyph sits at
    /// `(72, 0)` gives `(72, 0)` `1x1`, and an open six-slot tray at `(17, 1)`
    /// gives tile `1` at `(28, 1)` `8x3`.
    pub(crate) fn find_floating_pane_tray_anchor_rect(
        &self,
        render_snapshot: &RenderSnapshot,
        committed_regions: &CommittedRegions,
        pane_id: PaneId,
    ) -> Option<Rect> {
        let viewer_chrome = self.build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id);
        let frame_layout =
            render_snapshot.build_painted_frame_layout(viewer_chrome, committed_regions);
        let pane_area_origin = committed_regions.solved_regions.pane_rect.origin;
        if self.floating_pane_tray.is_none() {
            let glyph_cell = find_floating_pane_tray_glyph_cell(frame_layout)?;
            return Some(Rect::from_origin_and_size(
                Point {
                    column: glyph_cell.column.checked_sub(pane_area_origin.column)?,
                    row: 0,
                },
                Size {
                    column_count: 1,
                    row_count: 1,
                },
            ));
        }
        let tile_index = render_snapshot
            .session_snapshot
            .find_floating_pane_index(pane_id)?;
        let tile_rect = *compute_floating_pane_tray_layout(frame_layout)?
            .slot_rects
            .get(tile_index)?;
        Some(Rect::from_origin_and_size(
            Point {
                column: tile_rect
                    .origin
                    .column
                    .checked_sub(pane_area_origin.column)?,
                row: tile_rect.origin.row.checked_sub(pane_area_origin.row)?,
            },
            tile_rect.size,
        ))
    }

    /// Answer `mouse_input` against `mouse_frame`, the frame this viewer last
    /// painted, while the tray is open. Returns the actions to send, or `None`
    /// when the event takes the normal mouse path. Every event takes the
    /// normal mouse path while the tray is closed, where a left press on the
    /// tray button opens the tray.
    ///
    /// The event is placed against the tray as this viewer holds it:
    ///
    /// - A pointer motion sets the hovered tile to the tile under the pointer,
    ///   or to none. Over the tray it also clears the hovered pane and the
    ///   placement handle hover, and sends nothing.
    /// - A left press on a tile selects it and sends its
    ///   [`build_tray_tile_toggle_command`]. A left press on the header label
    ///   sends [`build_minimize_all_toggle_command`]. A left press on any other
    ///   cell of the tray sends nothing.
    /// - A press on the tray button closes the tray and sends nothing.
    /// - A press outside the tray and its button closes the tray and returns
    ///   `None`: the press then acts as it does with no tray.
    /// - Every other event over the tray is consumed and sends nothing. Off
    ///   the tray it returns `None`.
    pub(crate) fn handle_floating_pane_tray_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Option<Vec<MouseAction>> {
        let floating_pane_tray = self.floating_pane_tray?;
        let hit_region = resolve_hit_region(
            build_tray_frame_layout(mouse_frame, floating_pane_tray),
            mouse_input.position,
        );
        let is_on_tray = matches!(
            hit_region,
            HitRegion::FloatingPaneTrayTile { .. }
                | HitRegion::FloatingPaneTrayToggleMinimizeAll
                | HitRegion::FloatingPaneTray
        );
        match (mouse_input.mouse_kind, hit_region) {
            (MouseKind::Motion, _) => {
                let hovered_tile_index = match hit_region {
                    HitRegion::FloatingPaneTrayTile { tile_index } => Some(tile_index),
                    _ => None,
                };
                self.floating_pane_tray = Some(FloatingPaneTray {
                    hovered_tile_index,
                    ..floating_pane_tray
                });
                if !is_on_tray {
                    return None;
                }
                self.hovered_pane_id = None;
                self.placement_handle_hover = None;
                Some(Vec::new())
            }
            (
                MouseKind::Press(MouseButton::Left),
                HitRegion::FloatingPaneTrayTile { tile_index },
            ) => {
                self.floating_pane_tray = Some(FloatingPaneTray {
                    selected_tile_index: tile_index,
                    ..floating_pane_tray
                });
                Some(
                    mouse_frame
                        .session_snapshot
                        .floating_panes
                        .get(tile_index)
                        .map(|floating_pane| {
                            MouseAction::Command(Box::new(build_tray_tile_toggle_command(
                                floating_pane,
                            )))
                        })
                        .into_iter()
                        .collect(),
                )
            }
            (MouseKind::Press(MouseButton::Left), HitRegion::FloatingPaneTrayToggleMinimizeAll) => {
                Some(vec![MouseAction::Command(Box::new(
                    build_minimize_all_toggle_command(&mouse_frame.session_snapshot),
                ))])
            }
            (MouseKind::Press(_), HitRegion::FloatingPaneTrayButton) => {
                self.close_floating_pane_tray();
                Some(Vec::new())
            }
            (MouseKind::Press(_), _) if !is_on_tray => {
                self.close_floating_pane_tray();
                None
            }
            _ => is_on_tray.then(Vec::new),
        }
    }
}

/// The command that toggles `floating_pane` for the viewer:
/// `SetPaneMinimized { is_minimized: false }` while the viewer minimized the
/// pane, and `SetPaneMinimized { is_minimized: true }` otherwise. The session
/// restores a minimized pane on top and focuses it.
#[must_use]
fn build_tray_tile_toggle_command(floating_pane: &FloatingPane) -> Command {
    Command::SetPaneMinimized(SetPaneMinimizedArgs {
        pane_id: Some(floating_pane.pane_id),
        is_minimized: !floating_pane.is_minimized,
    })
}

/// The command that `core:toggle-minimize-all-floating-panes` sends for the
/// viewer of `session_snapshot`: `SetAllFloatingPanesMinimized {
/// is_minimized: false }` while the viewer minimized every floating pane
/// ([`SessionSnapshot::are_all_floating_panes_minimized`]), and
/// `SetAllFloatingPanesMinimized { is_minimized: true }` otherwise.
#[must_use]
pub(crate) fn build_minimize_all_toggle_command(session_snapshot: &SessionSnapshot) -> Command {
    Command::SetAllFloatingPanesMinimized(SetAllFloatingPanesMinimizedArgs {
        is_minimized: !session_snapshot.are_all_floating_panes_minimized(),
    })
}

/// `mouse_frame`'s frame layout with `floating_pane_tray` open: the viewer
/// chrome it was painted with, the tray set to `floating_pane_tray`, and the
/// input mode set to [`LockMode::FloatingPaneTray`].
fn build_tray_frame_layout(
    mouse_frame: &MouseFrame,
    floating_pane_tray: FloatingPaneTray,
) -> FrameLayout<'_> {
    let mut frame_layout = mouse_frame.build_frame_layout();
    frame_layout.viewer_chrome = ViewerChrome {
        active_input_mode: Some(LockMode::FloatingPaneTray),
        floating_pane_tray: Some(floating_pane_tray),
        ..mouse_frame.viewer_chrome
    };
    frame_layout
}

/// Where `mouse_frame` lays out the tray `floating_pane_tray`
/// ([`build_tray_frame_layout`], [`compute_floating_pane_tray_layout`]), or
/// `None` when the frame has no room for it.
fn find_floating_pane_tray_layout(
    mouse_frame: &MouseFrame,
    floating_pane_tray: FloatingPaneTray,
) -> Option<FloatingPaneTrayLayout> {
    compute_floating_pane_tray_layout(build_tray_frame_layout(mouse_frame, floating_pane_tray))
}
