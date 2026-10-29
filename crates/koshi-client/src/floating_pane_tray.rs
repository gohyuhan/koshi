//! The viewer's floating pane tray: opening and closing it, the tile its keys
//! act on, and the command a key or a click on it sends.
//!
//! The tray is this viewer's alone. Opening, closing and selecting send
//! nothing. A pane's tile sends `TogglePaneMinimized` for that pane, and the
//! header label sends `ToggleAllFloatingPanesMinimized`. The session reads
//! this client's view when it runs each, and changes the view of this client
//! only. The `+ new` tile closes the tray and sends the `NewPane` of
//! `core:new-pane-floating`.

use koshi_core::command::{Command, TogglePaneMinimizedArgs};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{CommandId, PaneId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::resolve::build_new_floating_pane_command;
use koshi_renderer::floating_pane_tray::{
    compute_floating_pane_tray_layout, compute_tray_tile_count, find_floating_pane_tray_glyph_cell,
    FloatingPaneTrayLayout,
};
use koshi_renderer::snapshot::{
    CommittedRegions, FloatingPaneTray, FrameLayout, LinkStatus, MouseFrame, RenderSnapshot,
    SessionSnapshot, ViewerChrome,
};
use koshi_renderer::{resolve_hit_region, HitRegion};

use crate::mouse::{build_frame_layout_without_overlays, MouseAction};
use crate::{Client, PlacementInputAction};

#[cfg(test)]
pub(crate) mod tests;

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
    /// has no room for the tray ([`compute_floating_pane_tray_layout`]).
    ///
    /// With floating panes `htop`, `logs` and `notes` and `logs` focused, the
    /// tray opens with tile `1` selected. With no floating pane, it opens with
    /// tile `0`, the `+ new` tile, selected.
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
    /// a slot that shows no tile ([`compute_tray_tile_count`]), selects
    /// nothing new. Does nothing while the tray is closed or with no painted
    /// frame.
    ///
    /// In a tray of six slots per row with eight floating panes, `Down` from
    /// tile `1` selects tile `7`, `Down` from tile `2` selects tile `8`, the
    /// `+ new` tile, and `Down` from tile `3` stays on tile `3`.
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
        let tile_count = compute_tray_tile_count(mouse_frame.session_snapshot.floating_panes.len());
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

    /// The command of the selected tile ([`activate_tray_tile`](Self::activate_tray_tile)),
    /// sent as a key binding's command. `Consumed` while the tray is closed,
    /// with no painted frame, or when the selected slot shows no tile.
    pub(crate) fn activate_selected_tray_tile(
        &mut self,
        mouse_frame: Option<&MouseFrame>,
    ) -> PlacementInputAction {
        let (Some(floating_pane_tray), Some(mouse_frame)) = (self.floating_pane_tray, mouse_frame)
        else {
            return PlacementInputAction::Consumed;
        };
        let Some(command) = self.activate_tray_tile(
            &mouse_frame.session_snapshot,
            floating_pane_tray.selected_tile_index,
        ) else {
            return PlacementInputAction::Consumed;
        };
        PlacementInputAction::SubmitCommand {
            command_id: CommandId::new(),
            command,
        }
    }

    /// The command tile `tile_index` of the open tray sends for
    /// `session_snapshot`'s floating panes. A pane's tile gives
    /// `TogglePaneMinimized` for that pane: the session minimizes the pane
    /// while this client shows it, and restores it on top and focuses it while
    /// this client minimized it. The `+ new` tile closes the tray
    /// ([`close_floating_pane_tray`](Self::close_floating_pane_tray)) and gives
    /// the `NewPane` of `core:new-pane-floating`
    /// ([`build_new_floating_pane_command`]). `None`, changing nothing, when
    /// the slot shows no tile ([`compute_tray_tile_count`]).
    ///
    /// With floating panes `htop` and `logs`, tile `1` gives the toggle of
    /// `logs`, tile `2` closes the tray and gives the `NewPane`, and tile `3`
    /// gives `None`.
    fn activate_tray_tile(
        &mut self,
        session_snapshot: &SessionSnapshot,
        tile_index: usize,
    ) -> Option<Command> {
        if let Some(floating_pane) = session_snapshot.floating_panes.get(tile_index) {
            return Some(Command::TogglePaneMinimized(TogglePaneMinimizedArgs {
                pane_id: Some(floating_pane.get_pane_id()),
            }));
        }
        if tile_index >= compute_tray_tile_count(session_snapshot.floating_panes.len()) {
            return None;
        }
        self.close_floating_pane_tray();
        Some(build_new_floating_pane_command())
    }

    /// Keep the open tray in step with `render_snapshot`, the frame just
    /// applied, painted with `committed_regions`: a selected tile index past
    /// the last tile ([`compute_tray_tile_count`]) moves to the last tile, and
    /// a hovered tile index past it is dropped. The tray closes
    /// ([`close_floating_pane_tray`](Self::close_floating_pane_tray)) when
    /// the frame has no room for it ([`compute_floating_pane_tray_layout`]):
    /// every pane is suppressed, or the pane area is too small. Does nothing
    /// while the tray is closed.
    ///
    /// Returns where the frame lays out the open tray, or `None` while the
    /// tray is closed, this call closing it included.
    ///
    /// With the tray open on tile `3`, the `+ new` tile after three floating
    /// panes, a frame with one floating pane selects tile `1`, its `+ new`
    /// tile.
    pub(crate) fn update_floating_pane_tray(
        &mut self,
        render_snapshot: &RenderSnapshot,
        committed_regions: &CommittedRegions,
    ) -> Option<FloatingPaneTrayLayout> {
        let floating_pane_tray = self.floating_pane_tray?;
        let tile_count =
            compute_tray_tile_count(render_snapshot.session_snapshot.floating_panes.len());
        self.floating_pane_tray = Some(FloatingPaneTray {
            selected_tile_index: floating_pane_tray.selected_tile_index.min(tile_count - 1),
            hovered_tile_index: floating_pane_tray
                .hovered_tile_index
                .filter(|&hovered_tile_index| hovered_tile_index < tile_count),
        });
        let viewer_chrome = self.build_viewer_chrome(
            render_snapshot.client_snapshot.active_tab_id,
            committed_regions,
        );
        let frame_layout =
            render_snapshot.build_painted_frame_layout(viewer_chrome, committed_regions);
        let floating_pane_tray_layout = compute_floating_pane_tray_layout(frame_layout);
        if floating_pane_tray_layout.is_none() {
            self.close_floating_pane_tray();
        }
        floating_pane_tray_layout
    }

    /// Where the floating panes of `render_snapshot`, painted with
    /// `committed_regions`, minimize into and restore out of. While this
    /// viewer's tray is open, it is the tray at `open_floating_pane_tray_layout`,
    /// the layout [`update_floating_pane_tray`](Self::update_floating_pane_tray)
    /// returns for that frame, and `None` when that layout is `None`. While
    /// the tray is closed, it is the pane-area cell under the tray button's
    /// glyph `⧉`, and `None` when the frame draws no tray button or the glyph
    /// sits left of the pane area.
    ///
    /// Over a pane area at `(0, 1)`, a closed tray whose glyph sits at
    /// `(72, 0)` gives the cell `(72, 0)` `1x1`.
    pub(crate) fn find_floating_pane_tray_anchor(
        &self,
        render_snapshot: &RenderSnapshot,
        committed_regions: &CommittedRegions,
        open_floating_pane_tray_layout: Option<FloatingPaneTrayLayout>,
    ) -> Option<FloatingPaneTrayAnchor> {
        let pane_area_origin = committed_regions.solved_regions.pane_rect.origin;
        if self.floating_pane_tray.is_some() {
            return open_floating_pane_tray_layout.map(|tray_layout| {
                FloatingPaneTrayAnchor::OpenTray {
                    tray_layout,
                    pane_area_origin,
                }
            });
        }
        let viewer_chrome = self.build_viewer_chrome(
            render_snapshot.client_snapshot.active_tab_id,
            committed_regions,
        );
        let glyph_cell = find_floating_pane_tray_glyph_cell(
            render_snapshot.build_painted_frame_layout(viewer_chrome, committed_regions),
        )?;
        Some(FloatingPaneTrayAnchor::ClosedTrayGlyphCell(
            Rect::from_origin_and_size(
                Point {
                    column: glyph_cell.column.checked_sub(pane_area_origin.column)?,
                    row: 0,
                },
                Size {
                    column_count: 1,
                    row_count: 1,
                },
            ),
        ))
    }

    /// Answer `mouse_input` against `mouse_frame`, the frame this viewer last
    /// painted, while the tray is open. Returns the actions to send, or `None`
    /// when the event takes the normal mouse path. Every event takes the
    /// normal mouse path while the tray is closed, where a left press on the
    /// tray button opens the tray.
    ///
    /// The event is placed against the tray as this viewer holds it, where a
    /// frame with the tray open draws it ([`build_tray_frame_layout`]). A
    /// press off the tray is placed against `mouse_frame` as it was painted,
    /// with no tray ([`build_frame_layout_without_overlays`]):
    ///
    /// - A pointer motion sets the hovered tile to the tile under the pointer,
    ///   or to none. Over the tray it also clears the hovered pane and the
    ///   placement handle hover, and sends nothing.
    /// - A left press on a tile selects it and sends its command
    ///   ([`activate_tray_tile`](Self::activate_tray_tile)): a pane's tile
    ///   sends `TogglePaneMinimized`, and the `+ new` tile closes the tray and
    ///   sends `NewPane`. A left press on the titlebar label sends
    ///   `ToggleAllFloatingPanesMinimized`. A left press on the
    ///   close button `✕` closes the tray and sends nothing. A left press on
    ///   any other cell of the tray sends nothing.
    /// - A press on the tray button `mouse_frame` shows closes the tray and
    ///   sends nothing. Two left presses on that button in one read, before a
    ///   paint, open the tray and close it.
    /// - A press outside the tray and that button closes the tray and returns
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
                | HitRegion::FloatingPaneTrayClose
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
                let mouse_actions =
                    match self.activate_tray_tile(&mouse_frame.session_snapshot, tile_index) {
                        Some(command) => vec![MouseAction::Command(Box::new(command))],
                        None => Vec::new(),
                    };
                Some(mouse_actions)
            }
            (MouseKind::Press(MouseButton::Left), HitRegion::FloatingPaneTrayToggleMinimizeAll) => {
                Some(vec![MouseAction::Command(Box::new(
                    Command::ToggleAllFloatingPanesMinimized,
                ))])
            }
            (MouseKind::Press(MouseButton::Left), HitRegion::FloatingPaneTrayClose) => {
                self.close_floating_pane_tray();
                Some(Vec::new())
            }
            (MouseKind::Press(_), _) if !is_on_tray => {
                self.close_floating_pane_tray();
                let painted_hit_region = resolve_hit_region(
                    build_frame_layout_without_overlays(mouse_frame),
                    mouse_input.position,
                );
                (painted_hit_region == HitRegion::FloatingPaneTrayButton).then(Vec::new)
            }
            _ => is_on_tray.then(Vec::new),
        }
    }
}

/// Where the floating panes of one frame minimize into and restore out of,
/// for this viewer ([`Client::find_floating_pane_tray_anchor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatingPaneTrayAnchor {
    /// The tray is closed: every pane uses this box, the cell under the tray
    /// button's glyph `⧉`, counted from the pane-area origin.
    ClosedTrayGlyphCell(Rect),
    /// The tray is open: each pane uses its tile in `tray_layout`, counted
    /// from `pane_area_origin`.
    OpenTray {
        /// Where the frame lays out the open tray, in screen cells.
        tray_layout: FloatingPaneTrayLayout,
        /// The pane area's top-left cell, in screen cells.
        pane_area_origin: Point,
    },
}

impl FloatingPaneTrayAnchor {
    /// The box the floating pane `pane_id` of `session_snapshot` minimizes
    /// into and restores out of, counted from the pane-area origin: the cell
    /// under the glyph for every pane while the tray is closed, and the
    /// pane's tile while it is open. `None` when the open tray holds no tile
    /// for the pane, or the tile sits left of or above the pane area.
    ///
    /// An open six-slot tray at `(5, 1)` over a pane area at `(0, 1)` gives
    /// tile `1` at `(18, 1)` `10x3`.
    pub(crate) fn find_anchor_rect(
        &self,
        session_snapshot: &SessionSnapshot,
        pane_id: PaneId,
    ) -> Option<Rect> {
        match self {
            Self::ClosedTrayGlyphCell(glyph_cell_rect) => Some(*glyph_cell_rect),
            Self::OpenTray {
                tray_layout,
                pane_area_origin,
            } => {
                let tile_index = session_snapshot.find_floating_pane_index(pane_id)?;
                let tile_rect = *tray_layout.slot_rects.get(tile_index)?;
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
        }
    }
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
