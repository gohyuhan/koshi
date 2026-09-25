//! What a mouse event means, decided by the viewer that received it.
//!
//! A viewer holds its own `mouse` and `copy` settings — how many lines one
//! wheel step scrolls, what the wheel does over a plain pane, whether a border
//! can be dragged, whether releasing a highlight copies it — so two viewers of
//! one session can answer the same event differently. It also holds a
//! [`MouseFrame`] of the frame it last painted, which says where every surface
//! sits, which line each pane's top visible row is, and which mouse modes each
//! pane's program had. That is everything a mouse event needs, so the viewer
//! decides and the session only executes.
//!
//! Mouse-select mode is the one piece of session state read from the viewer's
//! own copy rather than from the frame passed in: routing reads
//! [`Client::is_mouse_selection_enabled`]. An attached client moves that copy when a
//! frame paints, so a press routes the way the last painted frame said. A
//! viewer fed by a session subscription moves it from that subscription's
//! events as well.
//!
//! Every answer comes back from [`Client::handle_mouse`] as a list of
//! [`MouseAction`]: move a pane's view, hand the event to a pane's program,
//! send alternate-scroll arrows, move a pane border, or run a [`Command`]. The
//! binary's event loop passes each one to the session in the order it is given.
//!
//! The gesture state lives here too: the run of clicks, the pane a held button
//! is captured to, and the border, tab-strip, or selection drag under way. Each
//! is read and updated inside the single call that answers one event.
//!
//! What the viewer does **not** decide is how a forwarded event is encoded, how
//! far a scroll may travel, or which text a highlight covers. It names a pane
//! and a movement, or a pane and two grid positions; the session re-reads that
//! pane's live modes, its retained history, and its text at the moment it acts.
//!
//! **The frame is one event old.** A program that flips a mouse mode between
//! the last paint and this event is answered from the old modes once, and the
//! next frame corrects it. What is written is never wrong even then: the
//! session drops a forwarded event the live modes no longer ask for, and the
//! command path re-validates every command.
//!
//! **A press names an absolute line, not a screen row.** The frame says which
//! line each pane's top visible row is, so a press on the third visible row of
//! a pane whose top row is line 940 names line 942 — and it keeps naming line
//! 942 however much output arrives between the paint and the press.

use std::time::{Duration, Instant};

use ratatui::layout::Rect as RatatuiRect;

use koshi_config::types::WheelScroll;
use koshi_core::command::{
    ClearSelectionArgs, Command, CopyArgs, CopyTarget, FocusPaneArgs, FocusTabArgs, FocusTarget,
    GridPosition, PanePlacementAnchor, PanePlacementTarget, Selection, SelectionKind,
    SetSelectionArgs, TabTarget, VisualCommand,
};
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::mouse::{
    is_mouse_kind_reported, MouseButton, MouseInput, MouseKind, MouseTracking, ScrollDirection,
};
use koshi_ipc::placement::PanePlacementTabSnapshot;
use koshi_layout::placement::PlacementDestinations;
use koshi_renderer::snapshot::{MouseFrame, MousePane, PaneKind, PaneSlot, ViewerChrome};
use koshi_renderer::{
    compute_clamped_pane_cell, compute_content_rect, compute_pane_area,
    compute_placement_handle_rect, find_first_visible_tab_index, hit_test, pane_content_rect,
    HitRegion,
};

use crate::input::{
    build_placement_destinations, find_destination_tab_snapshot, is_same_tab_placement_noop,
};
use crate::terminal::project_core_rect;
use crate::{Client, PlacementInputAction};

#[cfg(test)]
mod tests;

/// How long after a press a second press still counts as a double click, and a
/// third as a triple.
///
/// A mouse reports a double click as two ordinary presses — no terminal
/// protocol carries a click count — so this gap is the only thing that tells one
/// from two deliberate clicks. Matches the 400ms alacritty settled on.
const DOUBLE_CLICK_THRESHOLD_DURATION: Duration = Duration::from_millis(400);

/// How often the view scrolls while a selection drag is held past a pane's top
/// or bottom edge. Matches alacritty's selection-scrolling interval.
const SELECTION_SCROLL_INTERVAL_DURATION: Duration = Duration::from_millis(15);

/// Lines the view scrolls per firing while a drag is held past an edge.
const SELECTION_SCROLL_LINE_COUNT: usize = 1;

/// Cells of horizontal drag that scroll the tab strip by one tab.
pub const TABLINE_DRAG_CELL_COUNT: i32 = 6;

/// Return whether a screen point lies in a ratatui rectangle.
fn is_point_in_ratatui_rect(screen_point: Point, target_rect: RatatuiRect) -> bool {
    u32::from(screen_point.column) >= u32::from(target_rect.x)
        && u32::from(screen_point.row) >= u32::from(target_rect.y)
        && u32::from(screen_point.column)
            < u32::from(target_rect.x).saturating_add(u32::from(target_rect.width))
        && u32::from(screen_point.row)
            < u32::from(target_rect.y).saturating_add(u32::from(target_rect.height))
}

/// What the viewer decided one wheel tick means: where the pointer is, and what
/// the session must do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelDecision {
    /// The pane under the pointer, or `None` over koshi's own chrome. The
    /// viewer marks it hovered so the renderer can color the wheel's target.
    pub hovered_pane_id: Option<PaneId>,
    /// What to do, or `None` when this tick does nothing — a horizontal wheel
    /// where only a vertical one acts, or a plain pane under a viewer whose
    /// `mouse.wheel` setting is `ignore`.
    pub mouse_action: Option<MouseAction>,
}

/// One thing the viewer wants the session to do for a mouse event. Every
/// variant names its target explicitly; the session hit-tests nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    /// Move this client's scrollback view of `pane` by `lines`, up into history
    /// or back down toward live output. A movement, not a position: how far the
    /// view may travel depends on the pane's retained history, which only the
    /// session knows.
    Scroll {
        /// The pane whose view moves.
        pane_id: PaneId,
        /// Up into history, or down toward live output.
        is_scrolling_up: bool,
        /// Lines to move, from this viewer's `mouse.scroll_line_count`.
        scroll_line_count: usize,
    },
    /// Hand the event to the program in `pane` as a mouse report. The session
    /// encodes it from that pane's live tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane_id: PaneId,
        /// The event, with the cell it landed on and the modifiers held.
        mouse_input: MouseInput,
    },
    /// Send `count` cursor arrow keys to `pane` — the alternate-scroll (`?1007`)
    /// translation of a wheel tick on the alternate screen.
    AltScrollArrows {
        /// The pane whose program receives the arrows.
        pane_id: PaneId,
        /// Up-arrows, or down-arrows.
        is_scrolling_up: bool,
        /// How many, from this viewer's `mouse.scroll_line_count`.
        arrow_count: usize,
    },
    /// Run `command` through the session's command door, attributed to this
    /// client's mouse. Focus and every selection change travel this way, so the
    /// session validates them exactly as it validates a command typed at the
    /// CLI.
    Command(Command),
    /// Move `pane`'s `side` border `count` cells, one cell per step, in the
    /// direction `step` names — `1` outward (the pane grows), `-1` inward.
    ///
    /// The session applies the steps one at a time and stops at the first it
    /// refuses, then reports how many it took; the viewer advances its drag
    /// anchor by exactly that many, so a pointer pushed past a pane's minimum
    /// size leaves the border pinned at that limit and a reverse drag moves it
    /// the instant the pointer crosses back.
    Resize {
        /// The pane whose border moves.
        pane_id: PaneId,
        /// Which of the pane's borders was grabbed.
        border_side: Direction,
        /// `1` grows the pane, `-1` shrinks it.
        resize_step: i16,
        /// How many single-cell steps the pointer travelled.
        requested_cell_count: u16,
    },
}

/// A selection drag under way: which pane it is in, the shape it makes, the end
/// that stays put, where the pointer last was, when the view must scroll next
/// because the pointer is being held past an edge, and which screen the pane was
/// showing when the drag started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionDrag {
    /// The pane being selected in. The drag stays with it even when the pointer
    /// leaves: a drag out of a pane extends to that pane's own edge, never to
    /// the neighbor the pointer moved onto.
    pane_id: PaneId,
    /// The shape the press picked — one click characters, two words, three
    /// lines, `Alt` a block. Fixed for the whole drag.
    selection_kind: SelectionKind,
    /// The end that stays put: the line and column the press landed on.
    anchor_grid_position: GridPosition,
    /// The pointer's last cell, in the viewer's own screen space. The scroll
    /// timer re-reads this to keep extending while the pointer is held still
    /// outside the pane.
    pointer_position: Point,
    /// When the view must next scroll because the pointer is being held past the
    /// pane's top or bottom edge; `None` whenever the pointer is inside.
    next_scroll_time: Option<Instant>,
    /// Whether the pane was showing the alternate screen when the press landed.
    /// The anchor names a line of that screen's text, so a frame reporting the
    /// other screen ends the drag.
    is_on_alternate_screen: bool,
}

/// A pane-border drag under way: the pane whose border was grabbed, which side
/// it is, and the cell the last *accepted* resize tracked to. The tracked cell
/// advances only over the cells a resize was accepted for, so pushing the
/// pointer past a pane's minimum size leaves the border pinned at that limit and
/// a reverse drag moves it the instant the pointer crosses back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeDrag {
    /// The pane whose border is being dragged.
    pane_id: PaneId,
    /// Which of the pane's borders was grabbed.
    border_side: Direction,
    /// The cell the last accepted resize tracked to; the next drag delta is
    /// measured from here.
    last_accepted_position: Point,
}

/// A tab-strip peek-drag under way: the column the drag anchored on and the
/// first visible tab index at that instant. Dragging horizontally from the
/// anchor scrolls the strip without changing which tab is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablineDrag {
    /// The screen column the drag anchored on.
    anchor_column: u16,
    /// The first visible tab index when the drag began.
    anchor_first_visible_tab_index: usize,
}

/// The run of clicks a press makes — one, two, or three — which is what picks
/// the shape of the selection a drag then makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickCount {
    /// A single click: a drag from here selects characters.
    Single,
    /// A double click: a drag from here selects whole words.
    Double,
    /// A triple click: a drag from here selects whole lines.
    Triple,
}

impl ClickCount {
    /// The selection shape a drag from a press with this run makes.
    fn get_selection_kind(self) -> SelectionKind {
        match self {
            ClickCount::Single => SelectionKind::Character,
            ClickCount::Double => SelectionKind::Word,
            ClickCount::Triple => SelectionKind::Line,
        }
    }
}

/// The most recent press: what was pressed, when, and the run it made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LastPress {
    /// The button that went down.
    button: MouseButton,
    /// When it went down.
    pressed_time: Instant,
    /// The run of clicks this press made.
    click_count: ClickCount,
}

/// The pane and button held by a forwarded mouse gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseCapture {
    /// The pane receiving the gesture.
    pane_id: PaneId,
    /// The button that began the gesture.
    button: MouseButton,
}

/// The tab and first visible tab index recorded by a tab-strip peek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablinePeek {
    /// The active tab when the peek was recorded.
    pub(crate) active_tab_id: TabId,
    /// The first tab shown by the peek.
    pub(crate) first_visible_tab_index: usize,
}

impl Client {
    /// The pane this viewer's pointer is over, where its tab strip is scrolled,
    /// and whether it is dialing the session again, for the frame it is about to
    /// paint or hit-test.
    ///
    /// A peek made on a tab other than `active_tab` is not applied, so a tab
    /// switch reveals the new tab. The peek is thrown away outright the moment
    /// the viewer sees a frame on another tab
    /// ([`note_active_tab`](Self::note_active_tab)), so switching back does not
    /// bring it out again.
    #[must_use]
    pub fn build_viewer_chrome(&self, active_tab_id: TabId) -> ViewerChrome {
        let is_pane_placement_visible = self.is_pane_placement_visible();
        ViewerChrome {
            hovered_pane_id: if is_pane_placement_visible {
                None
            } else {
                self.hovered_pane_id
            },
            placement_handle_pane_id: if is_pane_placement_visible {
                None
            } else {
                self.placement_handle_pane_id
            },
            active_input_mode: Some(self.get_active_input_mode()),
            is_pane_placement_visible,
            placement_source_pane_id: is_pane_placement_visible
                .then(|| self.get_placement_source_pane_id())
                .flatten(),
            tabline_offset: self
                .tabline_peek
                .filter(|tabline_peek| tabline_peek.active_tab_id == active_tab_id)
                .map(|tabline_peek| tabline_peek.first_visible_tab_index),
            reconnecting: self.reconnecting,
        }
    }

    /// Take in the tab a frame this viewer just looked at is showing, and throw
    /// away a tab-strip peek made on any other tab.
    ///
    /// The peek is the viewer's own: nothing on the session tells it a switch
    /// happened, so it learns from the frames it sees. A switch back starts
    /// fresh — peek from tab 3 while tab 0 is active, switch to tab 1, switch
    /// back to tab 0, and the strip starts at tab 0.
    pub fn note_active_tab(&mut self, active_tab_id: TabId) {
        self.tabline_peek = self
            .tabline_peek
            .filter(|tabline_peek| tabline_peek.active_tab_id == active_tab_id);
    }

    /// Route `mouse_input` while pane placement owns the mouse. Returns `None`
    /// when the event is not placement input and takes the normal mouse path.
    ///
    /// - While a submitted placement waits for the session, every event is
    ///   consumed and the placement drag ends.
    /// - With placement mode off, a left press on a placement handle opens
    ///   placement mode for that pane until the drag ends. Every other event
    ///   returns `None`.
    /// - With placement mode on, a left press on a tab previews that tab. A
    ///   left press on pane content, a placement handle, or a stack header
    ///   starts a drag, and picks that pane as the source unless it already is
    ///   the source. A pane picked in the session's active tab also takes
    ///   focus: a press on the header of collapsed `pane-123` returns a read
    ///   that focuses `pane-123`, which expands it in its stack. A pane picked
    ///   while another tab is previewed keeps focus where it is.
    /// - A left drag selects the target under the pointer: a swap, or an
    ///   insertion while Shift was held at the press. A left release after the
    ///   pointer moved submits the target under it. A release that submits
    ///   nothing cancels placement mode that lasts until its drag ends. Every
    ///   other event is consumed.
    pub(crate) fn handle_placement_mouse(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Option<PlacementInputAction> {
        if self.is_placement_confirmation_pending() {
            self.end_placement_drag();
            return Some(PlacementInputAction::Consumed);
        }
        let frame_layout = self.build_frame_layout(frame);
        let region = hit_test(frame_layout, mouse_input.position);
        if !self.is_placement_mode_active() {
            if let (MouseKind::Press(MouseButton::Left), HitRegion::PlacementHandle { pane_id }) =
                (mouse_input.mouse_kind, region)
            {
                let active_tab_id = frame.client_snapshot.active_tab_id;
                let Some((source_pane_id, destination_tab_id)) =
                    self.begin_mouse_placement_mode(pane_id, active_tab_id)
                else {
                    return Some(PlacementInputAction::Consumed);
                };
                self.begin_placement_drag(
                    mouse_input.position,
                    mouse_input
                        .modifier_flags
                        .has_all_modifiers(ModFlags::SHIFT),
                );
                return Some(PlacementInputAction::ReadPlacement {
                    pane_id_to_focus: Some(source_pane_id),
                    source_pane_id,
                    destination_tab_id,
                });
            }
            return None;
        }
        self.update_pointer_hover(frame, mouse_input.position, region);
        self.update_placement_tab_hover(
            match region {
                HitRegion::Tab { tab_id } => Some(tab_id),
                _ => None,
            },
            current_time,
        );
        let placement_action = match mouse_input.mouse_kind {
            MouseKind::Press(MouseButton::Left) => match region {
                HitRegion::Tab { tab_id } => self.select_placement_destination_tab(tab_id).map_or(
                    PlacementInputAction::Consumed,
                    |(source_pane_id, destination_tab_id)| PlacementInputAction::ReadPlacement {
                        pane_id_to_focus: None,
                        source_pane_id,
                        destination_tab_id,
                    },
                ),
                HitRegion::PlacementHandle { pane_id }
                | HitRegion::PaneContent { pane_id }
                | HitRegion::StackHeader { pane_id } => {
                    let displayed_tab_id = frame.client_snapshot.active_tab_id;
                    let is_displayed_tab_active = self.active_tab_id == Some(displayed_tab_id);
                    let placement_source_selection =
                        self.select_placement_source_pane(pane_id, displayed_tab_id);
                    self.begin_placement_drag(
                        mouse_input.position,
                        mouse_input
                            .modifier_flags
                            .has_all_modifiers(ModFlags::SHIFT),
                    );
                    placement_source_selection.map_or(
                        PlacementInputAction::Consumed,
                        |(source_pane_id, destination_tab_id)| {
                            PlacementInputAction::ReadPlacement {
                                pane_id_to_focus: is_displayed_tab_active.then_some(source_pane_id),
                                source_pane_id,
                                destination_tab_id,
                            }
                        },
                    )
                }
                HitRegion::PaneBorder { .. }
                | HitRegion::Tabline
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. }
                | HitRegion::Statusline
                | HitRegion::None => PlacementInputAction::Consumed,
            },
            MouseKind::Drag(MouseButton::Left) => {
                match region {
                    HitRegion::TablineScrollLeft { target_tab_index }
                    | HitRegion::TablineScrollRight { target_tab_index } => {
                        self.peek_tabline(frame, target_tab_index);
                    }
                    _ => {}
                }
                if self.has_placement_drag_moved(mouse_input.position) {
                    if let Some(placement_target) =
                        self.find_placement_target_at(mouse_input.position, frame)
                    {
                        self.set_mouse_placement_target(Some(placement_target));
                    } else {
                        self.clear_mouse_placement_target();
                    }
                } else {
                    self.clear_mouse_placement_target();
                }
                PlacementInputAction::Consumed
            }
            MouseKind::Scroll(scroll_direction) => {
                if let Some(target_tab_index) = self.tabline_step(frame, region, scroll_direction) {
                    self.peek_tabline(frame, target_tab_index);
                }
                PlacementInputAction::Consumed
            }
            MouseKind::Release(released_button) => {
                let should_submit = released_button == MouseButton::Left
                    && self.has_placement_drag_moved(mouse_input.position);
                if should_submit {
                    let placement_target =
                        self.find_placement_target_at(mouse_input.position, frame);
                    self.set_mouse_placement_target(placement_target);
                } else {
                    self.clear_mouse_placement_target();
                }
                self.end_placement_drag();
                let placement_action = if should_submit {
                    self.submit_placement_command()
                        .unwrap_or(PlacementInputAction::Consumed)
                } else {
                    PlacementInputAction::Consumed
                };
                if self.should_placement_mode_end_with_drag() {
                    self.cancel_placement_mode();
                }
                placement_action
            }
            MouseKind::Motion => PlacementInputAction::Consumed,
            _ => PlacementInputAction::Consumed,
        };
        Some(placement_action)
    }

    /// Clear the placement target. Does nothing while a submitted placement
    /// waits for the session.
    fn clear_mouse_placement_target(&mut self) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        placement_mode.placement_target = None;
    }

    /// Select `placement_target` from a placement drag. A target on the source
    /// pane, or one that leaves the source tab unchanged, clears the target.
    /// Does nothing while a submitted placement waits for the session.
    fn set_mouse_placement_target(&mut self, placement_target: Option<PanePlacementTarget>) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        let does_placement_target_preserve_layout =
            placement_target.as_ref().is_some_and(|placement_target| {
                self.get_placement_snapshot()
                    .is_some_and(|placement_snapshot| {
                        is_same_tab_placement_noop(placement_snapshot, placement_target)
                    })
            });
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        let is_target_source_pane = placement_mode.destination_tab_id
            == placement_mode.source_tab_id
            && placement_target.as_ref().is_some_and(|placement_target| {
                matches!(
                    placement_target,
                    PanePlacementTarget::Swap { target_pane_id }
                        | PanePlacementTarget::Split {
                            anchor: PanePlacementAnchor::Pane(target_pane_id),
                            ..
                        } if *target_pane_id == placement_mode.source_pane_id
                )
            });
        if is_target_source_pane || does_placement_target_preserve_layout {
            placement_mode.placement_target = None;
        } else {
            placement_mode.placement_target = placement_target;
        }
    }

    /// Return the target directly under the pointer in the retained destination
    /// snapshot. Animation does not change this target geometry.
    fn find_placement_target_at(
        &self,
        screen_point: Point,
        frame: &MouseFrame,
    ) -> Option<PanePlacementTarget> {
        let placement_mode = self.placement_state.placement_mode.as_ref()?;
        let placement_snapshot = self.get_placement_snapshot()?;
        if placement_snapshot.destination_tab_id != placement_mode.destination_tab_id {
            return None;
        }
        let tab_snapshot =
            find_destination_tab_snapshot(placement_snapshot, placement_mode.destination_tab_id)?;
        if tab_snapshot.is_every_pane_suppressed {
            return None;
        }
        let viewport_size = frame.committed_regions.viewport_size;
        let render_area =
            RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count);
        let destination_layout_rect = compute_content_rect(
            compute_pane_area(&frame.committed_regions, render_area),
            tab_snapshot.effective_cell_size,
        );
        if !is_point_in_ratatui_rect(screen_point, destination_layout_rect) {
            return None;
        }
        if tab_snapshot.effective_cell_size.column_count == 0
            || tab_snapshot.effective_cell_size.row_count == 0
        {
            return None;
        }
        let hovered_pane_id =
            Self::find_placement_pane_id_at(screen_point, destination_layout_rect, tab_snapshot);
        if hovered_pane_id == Some(placement_mode.source_pane_id) {
            return None;
        }
        let placement_drag = self.placement_state.placement_drag?;
        let destination_tab_id = placement_mode.destination_tab_id;
        let placement_destinations = build_placement_destinations(
            tab_snapshot,
            placement_mode.source_pane_id,
            placement_snapshot.pane_sizing,
        );
        if placement_drag.is_insertion_drag {
            let hovered_insertion_pane_id = hovered_pane_id.filter(|target_pane_id| {
                placement_destinations
                    .insertion_spans
                    .iter()
                    .any(|insertion_span| {
                        insertion_span.anchor == PanePlacementAnchor::Pane(*target_pane_id)
                    })
            });
            let Some(target_pane_id) = hovered_insertion_pane_id else {
                return find_mouse_insertion_target(
                    screen_point,
                    destination_layout_rect,
                    tab_snapshot.effective_cell_size,
                    destination_tab_id,
                    &placement_destinations,
                );
            };
            let target_rect = tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == target_pane_id)
                .and_then(|pane_slot| {
                    project_core_rect(
                        pane_slot.outer_rect,
                        tab_snapshot.effective_cell_size,
                        destination_layout_rect,
                    )
                })?;
            return Some(PanePlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Pane(target_pane_id),
                direction: compute_nearest_edge_direction(screen_point, target_rect),
            });
        }

        let target_pane_id = hovered_pane_id?;
        let swap_slot = placement_destinations
            .swap_slots
            .iter()
            .find(|swap_slot| swap_slot.pane_id == target_pane_id)?;
        Some(PanePlacementTarget::Swap {
            target_pane_id: swap_slot.pane_id,
        })
    }

    /// Find the pane at `screen_point` in the retained destination layout drawn
    /// at `placement_layout_rect`. A stack header resolves to its pane. After
    /// `pane-123` swaps with `pane-456`, the original `pane-456` slot still
    /// resolves to `pane-456` while the display animates. `screen_point` must
    /// lie inside `placement_layout_rect`.
    fn find_placement_pane_id_at(
        screen_point: Point,
        placement_layout_rect: RatatuiRect,
        placement_tab_snapshot: &PanePlacementTabSnapshot,
    ) -> Option<PaneId> {
        let layout_point = Point {
            column: screen_point.column.saturating_sub(placement_layout_rect.x),
            row: screen_point.row.saturating_sub(placement_layout_rect.y),
        };
        for stack_header in &placement_tab_snapshot.stack_headers {
            if stack_header.header_rect.is_point_inside(layout_point) {
                return Some(stack_header.pane_id);
            }
        }
        placement_tab_snapshot
            .pane_slots
            .iter()
            .filter(|pane_slot| pane_slot.is_visible)
            .find(|pane_slot| pane_slot.outer_rect.is_point_inside(layout_point))
            .map(|pane_slot| pane_slot.pane_id)
    }

    /// Decide what `mouse_input` means against `frame`, the last frame
    /// this viewer painted, and return everything the session must do about it,
    /// in order.
    ///
    /// A **wheel tick** is answered by [`handle_mouse_wheel`](Self::handle_mouse_wheel).
    /// The rest is routed by what the press began: a border drag resizes, a
    /// tab-strip drag scrolls the strip, a content drag highlights, and anything
    /// else is the program's. A **left press** acts on the region it landed on —
    /// a tab focuses that tab, a scroll arrow peeks the strip, a stack header
    /// focuses that pane, pane content focuses the pane or begins a highlight,
    /// a border begins a resize, the bare tab strip begins a peek-drag. A
    /// **release** ends whichever drag was under way. A buttonless **move**
    /// updates which pane is hovered and reaches the program if it asked for
    /// moves.
    ///
    /// A left press on the content of an already-focused plain shell at the
    /// pane's third visible row, with the pane's top row on line 940, arms a
    /// character drag anchored at line 942 and dispatches nothing yet.
    pub fn handle_mouse(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        self.note_active_tab(frame.client_snapshot.active_tab_id);
        self.drop_gestures_the_frame_ended(frame);
        match mouse_input.mouse_kind {
            MouseKind::Scroll(_) => match self.handle_mouse_wheel(mouse_input, frame) {
                Some(decision) => {
                    self.hovered_pane_id = decision.hovered_pane_id;
                    decision.mouse_action.into_iter().collect()
                }
                None => Vec::new(),
            },
            MouseKind::Press(MouseButton::Left) => {
                self.left_press(mouse_input, frame, current_time)
            }
            MouseKind::Drag(MouseButton::Left) => self.left_drag(mouse_input, frame, current_time),
            MouseKind::Release(_) => self.release_mouse_gesture(mouse_input, frame),
            MouseKind::Motion => {
                let hit_region = hit_test(self.build_frame_layout(frame), mouse_input.position);
                self.update_pointer_hover(frame, mouse_input.position, hit_region);
                self.forward_mouse_input(mouse_input, frame)
            }
            MouseKind::Press(_) | MouseKind::Drag(_) => {
                self.forward_mouse_input(mouse_input, frame)
            }
        }
    }

    /// How long until a selection drag held past a pane's edge must scroll the
    /// view again, or `None` when no drag is held there. The event loop blocks
    /// no longer than this, so a still pointer keeps pulling text in.
    #[must_use]
    pub fn next_mouse_wakeup(&self, current_time: Instant) -> Option<Duration> {
        self.selection_drag
            .and_then(|selection_drag| selection_drag.next_scroll_time)
            .map(|next_scroll_time| next_scroll_time.saturating_duration_since(current_time))
    }

    /// Scroll a selection drag held past a pane's edge, if its next step is due
    /// at `current_time`.
    ///
    /// Each firing moves the view one line toward the pointer. The highlight is
    /// re-extended in [`note_scroll_applied`](Self::note_scroll_applied), once
    /// the session has said where the view landed — the extension has to cover
    /// the line the scroll just revealed, and only the session knows whether
    /// there was one.
    ///
    /// A step that is not due yet yields no actions and leaves the timer where
    /// it is. A pointer no longer outside the pane, and a pane the frame no
    /// longer carries, yield no actions and disarm; the next drag event arms it
    /// again.
    pub fn expire_mouse_scroll(
        &mut self,
        current_time: Instant,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(selection_drag) = self.selection_drag else {
            return Vec::new();
        };
        if selection_drag
            .next_scroll_time
            .is_none_or(|next_scroll_time| next_scroll_time > current_time)
        {
            return Vec::new();
        }
        let scroll_direction = self.edge_scroll_direction(
            frame,
            selection_drag.pane_id,
            selection_drag.pointer_position,
        );
        let view_top_row_index = find_mouse_pane(frame, selection_drag.pane_id)
            .map(|mouse_pane| mouse_pane.view_top_row_index);
        let (Some(scroll_direction), Some(view_top_row_index)) =
            (scroll_direction, view_top_row_index)
        else {
            // The pane moved out from under a pointer that is no longer outside
            // it; stop scrolling and let the next drag event drive.
            self.selection_drag = Some(SelectionDrag {
                next_scroll_time: None,
                ..selection_drag
            });
            return Vec::new();
        };
        self.selection_scroll_origin_row_index = Some(view_top_row_index);
        self.selection_drag = Some(SelectionDrag {
            next_scroll_time: Some(current_time + SELECTION_SCROLL_INTERVAL_DURATION),
            ..selection_drag
        });
        vec![MouseAction::Scroll {
            pane_id: selection_drag.pane_id,
            is_scrolling_up: scroll_direction < 0,
            scroll_line_count: SELECTION_SCROLL_LINE_COUNT,
        }]
    }

    /// Take in where the view landed after a scroll: `view_top_row_index` is the line `pane`'s
    /// view now shows on its top row, or `None` for a pane with no terminal.
    ///
    /// Only a scroll the edge timer asked for is answered here; a wheel tick's
    /// scroll yields nothing. A view that did not move has nowhere left to go —
    /// it sits on the oldest retained line, or at the live bottom — and the
    /// timer disarms. A view that did move re-extends the highlight from the
    /// pointer's last cell, which is still outside the pane, so the extension
    /// clamps to the edge row and the highlight grows by the line the scroll
    /// revealed.
    pub fn note_scroll_applied(
        &mut self,
        pane_id: PaneId,
        view_top_row_index: Option<u64>,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(origin_row_index) = self.selection_scroll_origin_row_index.take() else {
            return Vec::new();
        };
        let Some(selection_drag) = self
            .selection_drag
            .filter(|selection_drag| selection_drag.pane_id == pane_id)
        else {
            return Vec::new();
        };
        let Some(reported_view_top_row_index) =
            view_top_row_index.filter(|&view_top_row_index| view_top_row_index != origin_row_index)
        else {
            self.selection_drag = Some(SelectionDrag {
                next_scroll_time: None,
                ..selection_drag
            });
            return Vec::new();
        };
        let Some((column_index, row_index)) = compute_clamped_pane_cell(
            self.build_frame_layout(frame),
            pane_id,
            selection_drag.pointer_position,
        ) else {
            return Vec::new();
        };
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: selection_drag.selection_kind,
                    anchor: selection_drag.anchor_grid_position,
                    cursor: GridPosition {
                        row_index: reported_view_top_row_index + u64::from(row_index),
                        column_index,
                    },
                },
            }),
        ))]
    }

    /// Drop the selection gesture under way, if any, leaving the highlight it
    /// made. Called when a key reaches the pane's program: the input is the
    /// program's, so the gesture ends.
    pub fn end_mouse_selection(&mut self) {
        self.selection_drag = None;
    }

    /// Drop every gesture under way: the selection drag, the border drag, the
    /// tab-strip peek-drag, the pane a held button was captured to, and the
    /// placement drag. The highlight a selection drag already made stands.
    /// Placement mode that lasts until its drag ends is cancelled. Placement
    /// mode that lasts until Esc stays on and loses its unconfirmed target.
    ///
    /// The pane under the pointer, where the tab strip is peeked, and the line a
    /// pending edge-scroll was asked from are left as they are.
    pub fn end_mouse_gestures(&mut self) {
        self.selection_drag = None;
        self.resize_drag = None;
        self.tabline_drag = None;
        self.mouse_capture = None;
        self.placement_state.placement_drag = None;
        if self.should_placement_mode_end_with_drag() {
            self.cancel_placement_mode();
        } else if !self.is_placement_confirmation_pending() {
            if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
                placement_mode.placement_target = None;
            }
        }
    }

    /// Decide what wheel tick `mouse_input` means against `frame`, the last frame this
    /// viewer painted. Returns `None` when `mouse_input` is not a wheel tick.
    ///
    /// Over the tab strip the tick steps the strip. Elsewhere it targets the
    /// pane under the pointer, or this viewer's focused terminal pane when the
    /// pointer is over chrome, and that pane is answered by precedence:
    ///
    /// 1. a highlight in the pane makes the tick scroll koshi's own scrollback;
    /// 2. else a program asking for the mouse gets the tick as a report;
    /// 3. else an alternate-screen program with `?1007` on gets arrow keys;
    /// 4. else this viewer's
    ///    [`mouse.wheel`](koshi_config::types::MouseConfig::wheel) decides —
    ///    scroll koshi's scrollback (the default), or do nothing.
    ///
    /// A wheel up over a plain shell with `scroll_line_count = 3` yields
    /// `Scroll { pane, up: true, lines: 3 }`; the same tick over a `vim` in
    /// normal tracking yields `Forward`.
    ///
    /// A tick over the tab strip moves this viewer's own peek and yields no
    /// action: nothing on the session changes.
    #[must_use]
    pub fn handle_mouse_wheel(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
    ) -> Option<WheelDecision> {
        let MouseKind::Scroll(scroll_direction) = mouse_input.mouse_kind else {
            return None;
        };
        let hit_region = hit_test(self.build_frame_layout(frame), mouse_input.position);
        if let Some(target_first_visible_tab_index) =
            self.tabline_step(frame, hit_region, scroll_direction)
        {
            self.peek_tabline(frame, target_first_visible_tab_index);
            return Some(WheelDecision {
                hovered_pane_id: None,
                mouse_action: None,
            });
        }
        let hovered_pane_id = find_pane_under_region(hit_region);
        let mouse_action = hovered_pane_id
            .or_else(|| find_focused_terminal_pane(frame))
            .and_then(|pane_id| self.wheel_on_pane(mouse_input, scroll_direction, frame, pane_id));
        Some(WheelDecision {
            hovered_pane_id,
            mouse_action,
        })
    }

    /// Answer a wheel tick aimed at `pane` by the precedence
    /// [`handle_mouse_wheel`](Self::handle_mouse_wheel) documents.
    fn wheel_on_pane(
        &self,
        mouse_input: MouseInput,
        scroll_direction: ScrollDirection,
        frame: &MouseFrame,
        pane_id: PaneId,
    ) -> Option<MouseAction> {
        let scroll_line_count = usize::from(self.client_config.mouse.scroll_line_count);
        let mouse_pane = find_mouse_pane(frame, pane_id)?;
        if mouse_pane.has_selection {
            return build_scroll_action(pane_id, scroll_direction, scroll_line_count);
        }
        if is_mouse_kind_reported(mouse_pane.mouse_tracking, mouse_input.mouse_kind) {
            return Some(MouseAction::Forward {
                pane_id,
                mouse_input,
            });
        }
        if mouse_pane.is_on_alternate_screen && mouse_pane.is_alternate_scroll_enabled {
            return resolve_scrolling_up_for_direction(scroll_direction).map(|is_scrolling_up| {
                MouseAction::AltScrollArrows {
                    pane_id,
                    is_scrolling_up,
                    arrow_count: scroll_line_count,
                }
            });
        }
        match self.client_config.mouse.wheel {
            WheelScroll::ScrollScrollback => {
                build_scroll_action(pane_id, scroll_direction, scroll_line_count)
            }
            WheelScroll::Ignore => None,
        }
    }

    /// Act on a left press over the region it landed on.
    fn left_press(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        match hit_test(self.build_frame_layout(frame), mouse_input.position) {
            HitRegion::Tab { tab_id } => {
                // The click reveals the tab it names, so any peek is over.
                self.tabline_peek = None;
                vec![MouseAction::Command(Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(tab_id),
                    client_id: Some(self.client_id),
                }))]
            }
            HitRegion::TablineScrollLeft { target_tab_index }
            | HitRegion::TablineScrollRight { target_tab_index } => {
                self.peek_tabline(frame, target_tab_index);
                Vec::new()
            }
            HitRegion::StackHeader { pane_id } => vec![focus_pane(self.client_id, pane_id)],
            HitRegion::PaneContent { pane_id } => {
                self.press_pane_content(pane_id, mouse_input, frame, current_time)
            }
            HitRegion::PaneBorder { pane_id, side } => {
                // Only a real divider — one with a pane drawn beside it to
                // resize against — begins a drag.
                if self.client_config.mouse.can_resize_pane_border
                    && border_has_neighbor(frame, pane_id, side)
                {
                    self.resize_drag = Some(ResizeDrag {
                        pane_id,
                        border_side: side,
                        last_accepted_position: mouse_input.position,
                    });
                }
                Vec::new()
            }
            HitRegion::PlacementHandle { .. } => Vec::new(),
            HitRegion::Tabline => {
                // A frame carrying no first visible tab index begins no
                // peek-drag.
                if let Some(first_visible_tab_index) =
                    find_first_visible_tab_index(self.build_frame_layout(frame))
                {
                    self.tabline_drag = Some(TablineDrag {
                        anchor_column: mouse_input.position.column,
                        anchor_first_visible_tab_index: first_visible_tab_index,
                    });
                }
                Vec::new()
            }
            HitRegion::Statusline | HitRegion::None => Vec::new(),
        }
    }

    /// Route a left press on a pane's content: a press on a pane the viewer has
    /// not focused focuses it; a press on the pane it is already in goes through
    /// to the program when that program asked for mouse events, and otherwise
    /// begins a highlight. A first click focuses, a second acts.
    ///
    /// **A mouse-aware program keeps the mouse.** `vim`, `htop`, and `lazygit`
    /// turn mouse reporting on and act on clicks themselves, so a drag inside
    /// one is theirs; a plain shell asks for nothing, so a drag there is a
    /// highlight.
    ///
    /// **Mouse-select mode takes the mouse back.** With the viewer's
    /// `mouse-select` mode on, a drag begins a koshi highlight even over a
    /// mouse-aware program — the way to copy text out of a full-screen `vim` or
    /// `htop`. Holding `Shift` on the press also begins a highlight, even when
    /// the viewer's mode is off. The mode is read from
    /// [`Client::is_mouse_selection_enabled`], the viewer's own copy, and not from `frame`.
    fn press_pane_content(
        &mut self,
        pane_id: PaneId,
        mouse_input: MouseInput,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        if frame.client_snapshot.focused_pane_id != Some(pane_id) {
            return vec![focus_pane(self.client_id, pane_id)];
        }
        let mouse_tracking = find_mouse_pane(frame, pane_id)
            .map_or(MouseTracking::Off, |mouse_pane| mouse_pane.mouse_tracking);
        let is_shift_selection = mouse_input
            .modifier_flags
            .has_all_modifiers(ModFlags::SHIFT);
        if is_mouse_kind_reported(mouse_tracking, mouse_input.mouse_kind)
            && !self.is_mouse_selection_enabled
            && !is_shift_selection
        {
            return self.forward_mouse_input(mouse_input, frame);
        }
        let click_count = self.record_click(MouseButton::Left, current_time);
        self.begin_selection_drag(pane_id, mouse_input, click_count, frame)
    }

    /// Begin a selection drag in `pane_id`: record where it started and the
    /// shape `clicks` picked, drop any highlight the pane already had, and — for
    /// a double or triple click — highlight the word or line straight away.
    ///
    /// The press itself drops the old highlight, so a plain click — press and
    /// release with no movement — leaves the pane with no highlight at all.
    ///
    /// **Whether the press also highlights depends on the shape:**
    ///
    /// - One click names a *point*. It highlights nothing until a drag gives it
    ///   a second cell.
    /// - A double or triple click names *text on its own* — the word or line
    ///   under the pointer — and highlights it on the press.
    ///
    /// A drag afterwards extends from the same anchor either way.
    ///
    /// `Alt` held at the press makes it a block selection whatever the run of
    /// clicks was, and like a plain click it names a point until the pointer
    /// moves.
    fn begin_selection_drag(
        &mut self,
        pane_id: PaneId,
        mouse_input: MouseInput,
        click_count: ClickCount,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(anchor_grid_position) =
            self.find_grid_position_at_screen_point(frame, pane_id, mouse_input.position)
        else {
            return Vec::new();
        };
        let selection_kind = if mouse_input.modifier_flags.has_all_modifiers(ModFlags::ALT) {
            SelectionKind::Block
        } else {
            click_count.get_selection_kind()
        };
        let drag = SelectionDrag {
            pane_id,
            selection_kind,
            anchor_grid_position,
            pointer_position: mouse_input.position,
            next_scroll_time: None,
            is_on_alternate_screen: find_mouse_pane(frame, pane_id)
                .is_some_and(|mouse_pane| mouse_pane.is_on_alternate_screen),
        };
        self.selection_drag = Some(drag);
        let mut selection_actions = vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id }),
        ))];
        if matches!(selection_kind, SelectionKind::Word | SelectionKind::Line) {
            // Both ends are the press; the session grows them outward to the
            // whole word or line as it applies the highlight.
            selection_actions.extend(self.extend_selection(drag, mouse_input.position, frame));
        }
        selection_actions
    }

    /// Route a left drag by the gesture the press began.
    fn left_drag(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        if let Some(resize_drag) = self.resize_drag {
            return self.drag_resize_to(resize_drag, mouse_input.position);
        }
        if let Some(tabline_drag) = self.tabline_drag {
            self.drag_tabline_to(tabline_drag, frame, mouse_input.position.column);
            return Vec::new();
        }
        if let Some(selection_drag) = self.selection_drag {
            return self.drag_selection_to(
                selection_drag,
                mouse_input.position,
                frame,
                current_time,
            );
        }
        self.forward_mouse_input(mouse_input, frame)
    }

    /// End whichever drag was under way. A release that ends a koshi drag is
    /// koshi's; any other release belongs to the program under the pointer.
    ///
    /// **Releasing the selection IS the copy**, as zellij ships it: the viewer
    /// dispatches [`VisualCommand::Copy`] for the pane it was highlighting, and
    /// the session reads the highlighted text at that instant — while it is
    /// exactly what the user saw — and puts it on the clipboard. A viewer whose
    /// `copy.should_copy_on_select` is off holds the highlight and copies nothing.
    fn release_mouse_gesture(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let selection_drag = self.selection_drag.take();
        let is_resizing = self.resize_drag.take().is_some();
        let is_peeking = self.tabline_drag.take().is_some();
        if !(selection_drag.is_some() || is_resizing || is_peeking) {
            return self.forward_mouse_input(mouse_input, frame);
        }
        // A plain click, whose press highlighted nothing, has no highlight to
        // copy; the session finds none and copies nothing.
        match selection_drag.filter(|_| self.client_config.copy.should_copy_on_select) {
            Some(selection_drag) => vec![MouseAction::Command(Command::Visual(
                VisualCommand::Copy(CopyArgs {
                    pane_id: selection_drag.pane_id,
                    clipboard_target: resolve_clipboard_target(self.client_config.copy.clipboard),
                    should_trim_trailing_whitespace: self
                        .client_config
                        .copy
                        .should_trim_trailing_whitespace,
                }),
            ))],
            None => Vec::new(),
        }
    }

    /// Extend the selection drag to the pointer at `at`.
    ///
    /// A pointer inside the pane highlights from the anchor to the cell under
    /// it. A pointer past the top or bottom edge highlights to the pane's
    /// nearest row and arms the scroll timer, so holding it there keeps pulling
    /// more text in. A pointer only to the left or right of the pane clamps to
    /// the edge column without scrolling — there is no more text sideways.
    fn drag_selection_to(
        &mut self,
        selection_drag: SelectionDrag,
        position: Point,
        frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        let next_scroll_time = self
            .edge_scroll_direction(frame, selection_drag.pane_id, position)
            .map(|_| current_time + SELECTION_SCROLL_INTERVAL_DURATION);
        self.selection_drag = Some(SelectionDrag {
            pointer_position: position,
            next_scroll_time,
            ..selection_drag
        });
        self.extend_selection(selection_drag, position, frame)
    }

    /// Highlight from `drag`'s anchor to the pointer at `at`. Character and
    /// block drags mean exactly the cells named; a word or line drag is grown to
    /// whole words or lines by the session, which holds the text.
    fn extend_selection(
        &self,
        selection_drag: SelectionDrag,
        position: Point,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(cursor_grid_position) =
            self.find_grid_position_at_screen_point(frame, selection_drag.pane_id, position)
        else {
            return Vec::new();
        };
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id: selection_drag.pane_id,
                selection: Selection {
                    selection_kind: selection_drag.selection_kind,
                    anchor: selection_drag.anchor_grid_position,
                    cursor: cursor_grid_position,
                },
            }),
        ))]
    }

    /// Move the grabbed border to follow a drag whose pointer is now at `at`.
    ///
    /// Asks for the move one cell at a time toward the border, so a fast drag
    /// that jumps several cells fills right up to a pane's minimum size instead
    /// of being refused whole. The whole distance from the anchor is named every
    /// time; [`note_resize_applied`](Self::note_resize_applied) moves the anchor
    /// over the cells the session says it took.
    fn drag_resize_to(&self, resize_drag: ResizeDrag, pointer_position: Point) -> Vec<MouseAction> {
        let resize_cell_delta = compute_resize_cell_delta(
            resize_drag.border_side,
            resize_drag.last_accepted_position,
            pointer_position,
        );
        if resize_cell_delta == 0 {
            return Vec::new();
        }
        vec![MouseAction::Resize {
            pane_id: resize_drag.pane_id,
            border_side: resize_drag.border_side,
            resize_step: resize_cell_delta.signum(),
            requested_cell_count: resize_cell_delta.unsigned_abs(),
        }]
    }

    /// Advance the border drag's anchor over the `applied` cells the session
    /// accepted of a move asked for on `pane`'s `side` in direction `step`. The
    /// first refused step is the wall, so the anchor stops there and a reverse
    /// drag moves the border the instant the pointer crosses back.
    ///
    /// Nothing moves unless the drag now held is the one `pane` and `side` name:
    /// an answer for a border the viewer has let go of, or for another border of
    /// the same pane, leaves the anchor where it is.
    ///
    /// `step` and `applied` are the whole of the distance — the pointer is never
    /// read here, so an answer that lands while the pointer is still moves the
    /// anchor exactly as far as one that lands mid-motion.
    pub fn note_resize_applied(
        &mut self,
        pane_id: PaneId,
        border_side: Direction,
        resize_step: i16,
        applied_cell_count: u16,
    ) {
        let Some(resize_drag) = self.resize_drag.filter(|resize_drag| {
            resize_drag.pane_id == pane_id && resize_drag.border_side == border_side
        }) else {
            return;
        };
        if applied_cell_count > 0 {
            self.resize_drag = Some(ResizeDrag {
                last_accepted_position: advance_resize_anchor(
                    resize_drag.border_side,
                    resize_drag.last_accepted_position,
                    resize_step,
                    applied_cell_count,
                ),
                ..resize_drag
            });
        }
    }

    /// Capture the gesture `button` began in `pane`. The caller runs this for
    /// every press it forwards, as it forwards it.
    ///
    /// The capture is what carries the rest of the gesture: the drags and the
    /// release that follow go to this same pane even as the pointer leaves it,
    /// re-stamped with this button. A press that is never forwarded — koshi's
    /// own, or one the pane's tracking level does not ask for — captures
    /// nothing.
    pub fn note_press_forwarded(&mut self, pane_id: PaneId, button: MouseButton) {
        self.mouse_capture = Some(MouseCapture { pane_id, button });
    }

    /// Scroll the tab strip to follow an in-flight drag whose pointer is now at
    /// column `x`. Dragging right moves the strip right (revealing earlier
    /// tabs); one tab per [`TABLINE_DRAG_CELL_COUNT`] cells.
    fn drag_tabline_to(
        &mut self,
        tabline_drag: TablineDrag,
        frame: &MouseFrame,
        pointer_column: u16,
    ) {
        let tabline_scroll_cell_delta =
            i32::from(tabline_drag.anchor_column) - i32::from(pointer_column);
        let tabline_scroll_step_count = tabline_scroll_cell_delta / TABLINE_DRAG_CELL_COUNT;
        let target_first_visible_tab_index = (tabline_drag.anchor_first_visible_tab_index as i32
            + tabline_scroll_step_count)
            .max(0) as usize;
        self.peek_tabline(frame, target_first_visible_tab_index);
    }

    /// The tab index a wheel tick over the tab strip scrolls to, or `None` when
    /// the tick did not land on the strip. Up and left step toward the first
    /// tab, down and right toward the last.
    fn tabline_step(
        &self,
        frame: &MouseFrame,
        hit_region: HitRegion,
        scroll_direction: ScrollDirection,
    ) -> Option<usize> {
        if !matches!(
            hit_region,
            HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. }
        ) {
            return None;
        }
        let first_visible_tab_index = find_first_visible_tab_index(self.build_frame_layout(frame))?;
        Some(match scroll_direction {
            ScrollDirection::Up | ScrollDirection::Left => {
                first_visible_tab_index.saturating_sub(1)
            }
            ScrollDirection::Down | ScrollDirection::Right => first_visible_tab_index + 1,
        })
    }

    /// Peek this viewer's tab strip from tab index `target_first_visible_tab_index`, recorded against the
    /// tab the frame is showing so a subsequent tab switch cancels it. The renderer
    /// clamps an index past the last tab, so an over-far target is harmless.
    fn peek_tabline(&mut self, frame: &MouseFrame, target_first_visible_tab_index: usize) {
        self.tabline_peek = Some(TablinePeek {
            active_tab_id: frame.client_snapshot.active_tab_id,
            first_visible_tab_index: target_first_visible_tab_index,
        });
    }

    /// Record a press into this viewer's run of clicks and report what it makes:
    /// one click, two, or three.
    ///
    /// **The gap is the only thing that decides this.** A mouse reports a double
    /// click as two ordinary presses — no terminal protocol carries a click
    /// count — so the time between them is the only signal there is. Pressing a
    /// different button always starts a new run: a left click followed by a
    /// quick right click is not a double click.
    ///
    /// Press at `0ms` → [`Single`](ClickCount::Single); again at `120ms` →
    /// [`Double`](ClickCount::Double); again at `260ms` →
    /// [`Triple`](ClickCount::Triple); again at `900ms` → `Single`, the run
    /// having lapsed. A fourth press right after a `Triple` also starts over.
    fn record_click(&mut self, button: MouseButton, current_time: Instant) -> ClickCount {
        let click_count = match self.last_mouse_press {
            Some(last_mouse_press) if last_mouse_press.button != button => ClickCount::Single,
            Some(last_mouse_press)
                if current_time.duration_since(last_mouse_press.pressed_time)
                    >= DOUBLE_CLICK_THRESHOLD_DURATION =>
            {
                ClickCount::Single
            }
            Some(last_mouse_press) => match last_mouse_press.click_count {
                ClickCount::Single => ClickCount::Double,
                ClickCount::Double => ClickCount::Triple,
                ClickCount::Triple => ClickCount::Single,
            },
            None => ClickCount::Single,
        };
        self.last_mouse_press = Some(LastPress {
            button,
            pressed_time: current_time,
            click_count,
        });
        click_count
    }

    /// Hand `mouse_input` to the program in the pane it belongs to.
    ///
    /// A button gesture is captured: the press picks the focused pane under the
    /// pointer, and the drags and release that follow go to that same pane even
    /// as the pointer leaves it. The capture itself is recorded by
    /// [`Client::note_press_forwarded`], which the caller runs for every press
    /// this returns. A bare move goes to the focused pane. A drag or release
    /// with no capture — the press was koshi's, it focused nothing, or the
    /// pane's tracking level did not ask for it — is dropped, so no program
    /// ever sees a release without its press.
    ///
    /// A press or a bare move outside the pane's content reaches no program: it
    /// names no cell there. A captured drag or release does reach it, clamped to
    /// its nearest edge by the session, so a gesture that wandered off the pane
    /// still ends inside it.
    ///
    /// The pane's tracking level from the painted frame gates the forward_mouse_input, and
    /// the session re-reads the live level before it writes.
    fn forward_mouse_input(
        &mut self,
        mouse_input: MouseInput,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let mouse_capture = self.mouse_capture;
        // A release ends the capture, whether or not it forwards. Which button
        // released cannot be trusted (some terminals report every release as the
        // left button), so any release clears.
        if matches!(mouse_input.mouse_kind, MouseKind::Release(_)) {
            self.mouse_capture = None;
        }
        let (pane_id, mouse_kind) = match mouse_input.mouse_kind {
            MouseKind::Press(_) | MouseKind::Motion => match find_focused_terminal_pane(frame) {
                Some(focused_pane_id)
                    if pane_content_rect(self.build_frame_layout(frame), focused_pane_id)
                        .is_some_and(|content_rect| {
                            content_rect.is_point_inside(mouse_input.position)
                        }) =>
                {
                    (focused_pane_id, mouse_input.mouse_kind)
                }
                _ => return Vec::new(),
            },
            // A captured drag or release is re-stamped with the button its press
            // named — the event's own button is unreliable, so the program sees
            // the same button it saw go down.
            MouseKind::Drag(_) | MouseKind::Release(_) => match mouse_capture {
                Some(MouseCapture {
                    pane_id: captured_pane_id,
                    button: captured_button,
                }) => (
                    captured_pane_id,
                    replace_mouse_button(mouse_input.mouse_kind, captured_button),
                ),
                None => return Vec::new(),
            },
            MouseKind::Scroll(_) => return Vec::new(),
        };
        let Some(mouse_pane) = find_mouse_pane(frame, pane_id) else {
            return Vec::new();
        };
        if !is_mouse_kind_reported(mouse_pane.mouse_tracking, mouse_kind) {
            return Vec::new();
        }
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: MouseInput {
                mouse_kind,
                ..mouse_input
            },
        }]
    }

    /// Drop any gesture the newest frame ended.
    ///
    /// A gesture aimed at a pane the frame no longer draws is over: a pane that
    /// closed, was hidden, or left with a tab switch cannot be dragged or
    /// forwarded to, so the gesture ends where it stands.
    ///
    /// A selection drag also ends when its pane swapped between the primary and
    /// the alternate screen. Its anchor names a line of the screen the press
    /// landed on, and the other screen's rows are different text.
    fn drop_gestures_the_frame_ended(&mut self, frame: &MouseFrame) {
        let is_pane_drawn = |pane_id: PaneId| find_visible_pane_slot(frame, pane_id).is_some();
        self.selection_drag = self.selection_drag.filter(|selection_drag| {
            is_pane_drawn(selection_drag.pane_id)
                && find_mouse_pane(frame, selection_drag.pane_id).is_none_or(|mouse_pane| {
                    mouse_pane.is_on_alternate_screen == selection_drag.is_on_alternate_screen
                })
        });
        self.resize_drag = self
            .resize_drag
            .filter(|resize_drag| is_pane_drawn(resize_drag.pane_id));
        self.mouse_capture = self
            .mouse_capture
            .filter(|mouse_capture| is_pane_drawn(mouse_capture.pane_id));
        self.hovered_pane_id = self
            .hovered_pane_id
            .filter(|&pane_id| is_pane_drawn(pane_id));
        self.placement_handle_pane_id = self
            .placement_handle_pane_id
            .filter(|&pane_id| is_pane_drawn(pane_id));
    }

    /// Update the viewer-owned hover state from the region under the pointer.
    fn update_pointer_hover(
        &mut self,
        frame: &MouseFrame,
        screen_point: Point,
        hit_region: HitRegion,
    ) {
        let hovered_pane_id = match hit_region {
            HitRegion::PaneContent { pane_id }
            | HitRegion::PlacementHandle { pane_id }
            | HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
            } => Some(pane_id),
            _ => find_pane_under_region(hit_region),
        };
        self.hovered_pane_id = hovered_pane_id;
        self.placement_handle_pane_id = match hit_region {
            HitRegion::PlacementHandle { pane_id }
            | HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
            } => find_visible_pane_handle(frame, pane_id, screen_point),
            _ => None,
        };
    }

    /// The position in `pane`'s text that the screen cell `at` names, with a
    /// point outside the pane pulled to its nearest edge so a drag that left the
    /// pane still selects up to it.
    ///
    /// The row is absolute: the frame says which line the pane's top visible row
    /// is, and the `n`-th visible row is that line plus `n`. Absolute lines never
    /// move, so output arriving between the paint and the press cannot shift what
    /// the press names.
    fn find_grid_position_at_screen_point(
        &self,
        frame: &MouseFrame,
        pane_id: PaneId,
        screen_point: Point,
    ) -> Option<GridPosition> {
        let (column_index, row_index) =
            compute_clamped_pane_cell(self.build_frame_layout(frame), pane_id, screen_point)?;
        let view_top_row_index = find_mouse_pane(frame, pane_id)?.view_top_row_index;
        Some(GridPosition {
            row_index: view_top_row_index + u64::from(row_index),
            column_index,
        })
    }

    /// Which way the view must scroll for a drag held at `at`: `-1` above the
    /// pane's first row, `1` below its last, and `None` while the pointer is
    /// level with the pane.
    ///
    /// Only the vertical edges scroll. Past the left or right edge there is no
    /// further text to reach, so the highlight clamps to the edge column and
    /// stays put.
    fn edge_scroll_direction(
        &self,
        frame: &MouseFrame,
        pane_id: PaneId,
        screen_point: Point,
    ) -> Option<i8> {
        let content_rect = pane_content_rect(self.build_frame_layout(frame), pane_id)?;
        let bottom_row_index = content_rect
            .origin
            .row
            .saturating_add(content_rect.cell_size.row_count.saturating_sub(1));
        if screen_point.row < content_rect.origin.row {
            Some(-1)
        } else if screen_point.row > bottom_row_index {
            Some(1)
        } else {
            None
        }
    }

    /// `frame` borrowed for hit-testing, with this viewer's own `ViewerChrome` state.
    fn build_frame_layout<'a>(
        &self,
        frame: &'a MouseFrame,
    ) -> koshi_renderer::snapshot::FrameLayout<'a> {
        frame.build_frame_layout(self.build_viewer_chrome(frame.client_snapshot.active_tab_id))
    }
}

/// A `FocusPane` for `pane_id`, naming `client_id` so the switch moves that viewer's
/// focus and no other's.
fn focus_pane(client_id: ClientId, pane_id: PaneId) -> MouseAction {
    MouseAction::Command(Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(pane_id),
        client_id: Some(client_id),
    }))
}

/// The pane a hit-tested `region` sits in, or `None` when it is chrome. Only a
/// pane's own content counts as hovering that pane — the wheel scrolls it and
/// the renderer marks its border.
fn find_pane_under_region(hit_region: HitRegion) -> Option<PaneId> {
    match hit_region {
        HitRegion::PaneContent { pane_id } => Some(pane_id),
        _ => None,
    }
}

/// Return an insertion into the smallest insertion span under `screen_point`,
/// on that span's edge nearest the pointer. Between spans of equal area, the
/// earlier span wins. Returns `None` when no span covers `screen_point`.
fn find_mouse_insertion_target(
    screen_point: Point,
    destination_layout_rect: RatatuiRect,
    destination_cell_size: Size,
    destination_tab_id: TabId,
    placement_destinations: &PlacementDestinations,
) -> Option<PanePlacementTarget> {
    placement_destinations
        .insertion_spans
        .iter()
        .filter_map(|insertion_span| {
            let span_rect = project_core_rect(
                insertion_span.span_rect,
                destination_cell_size,
                destination_layout_rect,
            )?;
            is_point_in_ratatui_rect(screen_point, span_rect).then_some((insertion_span, span_rect))
        })
        .min_by_key(|(_, span_rect)| span_rect.area())
        .map(|(insertion_span, span_rect)| PanePlacementTarget::Split {
            destination_tab_id,
            anchor: insertion_span.anchor.clone(),
            direction: compute_nearest_edge_direction(screen_point, span_rect),
        })
}

/// Return the side of a screen rectangle nearest to the pointer.
fn compute_nearest_edge_direction(screen_point: Point, target_rect: RatatuiRect) -> Direction {
    let left_distance = screen_point.column.saturating_sub(target_rect.x);
    let right_edge = target_rect
        .x
        .saturating_add(target_rect.width.saturating_sub(1));
    let right_distance = right_edge.saturating_sub(screen_point.column);
    let top_distance = screen_point.row.saturating_sub(target_rect.y);
    let bottom_edge = target_rect
        .y
        .saturating_add(target_rect.height.saturating_sub(1));
    let bottom_distance = bottom_edge.saturating_sub(screen_point.row);
    let nearest_edge_distance = left_distance
        .min(right_distance)
        .min(top_distance)
        .min(bottom_distance);
    if nearest_edge_distance == top_distance {
        Direction::Up
    } else if nearest_edge_distance == bottom_distance {
        Direction::Down
    } else if nearest_edge_distance == left_distance {
        Direction::Left
    } else {
        Direction::Right
    }
}

/// Return the visible pane's outer rectangle in client screen coordinates.
fn find_pane_outer_screen_rect(
    frame: &MouseFrame,
    pane_id: PaneId,
) -> Option<koshi_core::geometry::Rect> {
    let content_rect =
        pane_content_rect(frame.build_frame_layout(ViewerChrome::default()), pane_id)?;
    Some(koshi_core::geometry::Rect::from_origin_and_size(
        Point {
            column: content_rect.origin.column.saturating_sub(1),
            row: content_rect.origin.row.saturating_sub(1),
        },
        Size {
            column_count: content_rect.cell_size.column_count.saturating_add(2),
            row_count: content_rect.cell_size.row_count.saturating_add(2),
        },
    ))
}

/// Return the pane id when the pointer is inside its painted placement handle.
fn find_visible_pane_handle(
    frame: &MouseFrame,
    pane_id: PaneId,
    screen_point: Point,
) -> Option<PaneId> {
    let pane_outer_rect = find_pane_outer_screen_rect(frame, pane_id)?;
    compute_placement_handle_rect(pane_outer_rect)
        .filter(|handle_rect| handle_rect.is_point_inside(screen_point))
        .map(|_| pane_id)
}

/// Whether the `side` border of `pane` has another pane drawn right beside it in
/// `frame` — the only kind of border a drag can move.
///
/// A neighbor's box starts exactly [`TabSnapshot::gap`] cells past the pane's
/// edge on that side and covers at least one of the same rows (or columns).
/// With `gap` 0, a pane at columns 0–39 next to one at 40–79 has a neighbor on
/// its right; with `gap` 2 the neighbor starts at column 42. The second pane's
/// right edge is the tab's outer frame and has none. A zoomed view draws one
/// pane and no dividers, and the boundary above a collapsed stack header has
/// no drawn pane on the far side; neither is draggable.
///
/// [`TabSnapshot::gap`]: koshi_renderer::snapshot::TabSnapshot::gap
fn border_has_neighbor(frame: &MouseFrame, pane_id: PaneId, border_side: Direction) -> bool {
    let Some(pane_outer_rect) =
        find_visible_pane_slot(frame, pane_id).map(|pane_slot| pane_slot.outer_rect)
    else {
        return false;
    };
    let gap_cell_count = frame.session_snapshot.active_tab_snapshot.gap_cell_count;
    frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .filter(|pane_slot| pane_slot.is_visible && pane_slot.pane_id != pane_id)
        .any(|pane_slot| {
            let neighbor_rect = pane_slot.outer_rect;
            match border_side {
                Direction::Right => {
                    neighbor_rect.origin.column
                        == (pane_outer_rect.origin.column + pane_outer_rect.cell_size.column_count)
                            .saturating_add(gap_cell_count)
                        && has_row_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Left => {
                    (neighbor_rect.origin.column + neighbor_rect.cell_size.column_count)
                        .saturating_add(gap_cell_count)
                        == pane_outer_rect.origin.column
                        && has_row_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Down => {
                    neighbor_rect.origin.row
                        == (pane_outer_rect.origin.row + pane_outer_rect.cell_size.row_count)
                            .saturating_add(gap_cell_count)
                        && has_column_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Up => {
                    (neighbor_rect.origin.row + neighbor_rect.cell_size.row_count)
                        .saturating_add(gap_cell_count)
                        == pane_outer_rect.origin.row
                        && has_column_overlap(pane_outer_rect, neighbor_rect)
                }
            }
        })
}

/// The box `frame` draws for `pane`, or `None` when the frame shows it nowhere —
/// a pane that closed, was hidden, or sits on a tab this frame is not showing.
fn find_visible_pane_slot(frame: &MouseFrame, pane_id: PaneId) -> Option<&PaneSlot> {
    frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.is_visible && pane_slot.pane_id == pane_id)
}

/// Whether two pane boxes cover any of the same rows.
fn has_row_overlap(
    first_pane_rect: koshi_core::geometry::Rect,
    second_pane_rect: koshi_core::geometry::Rect,
) -> bool {
    first_pane_rect.origin.row < second_pane_rect.origin.row + second_pane_rect.cell_size.row_count
        && second_pane_rect.origin.row
            < first_pane_rect.origin.row + first_pane_rect.cell_size.row_count
}

/// Whether two pane boxes cover any of the same columns.
fn has_column_overlap(
    first_pane_rect: koshi_core::geometry::Rect,
    second_pane_rect: koshi_core::geometry::Rect,
) -> bool {
    first_pane_rect.origin.column
        < second_pane_rect.origin.column + second_pane_rect.cell_size.column_count
        && second_pane_rect.origin.column
            < first_pane_rect.origin.column + first_pane_rect.cell_size.column_count
}

/// The frame's entry for `pane`, or `None` when the frame carried no content for
/// it.
fn find_mouse_pane(frame: &MouseFrame, pane_id: PaneId) -> Option<&MousePane> {
    frame
        .mouse_panes
        .iter()
        .find(|mouse_pane| mouse_pane.pane_id == pane_id)
}

/// This client's focused pane in `frame` when it is a terminal — the pane an
/// event over chrome falls through to. A plugin pane has no program to answer
/// and no scrollback to move, so it is `None`.
fn find_focused_terminal_pane(frame: &MouseFrame) -> Option<PaneId> {
    let focused_pane_id = frame.client_snapshot.focused_pane_id?;
    frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .any(|pane_slot| {
            pane_slot.pane_id == focused_pane_id
                && matches!(pane_slot.pane_kind, PaneKind::Terminal)
        })
        .then_some(focused_pane_id)
}

/// A scrollback movement for a vertical tick; a horizontal tick moves no
/// vertical view, so it yields `None`.
fn build_scroll_action(
    pane_id: PaneId,
    scroll_direction: ScrollDirection,
    scroll_line_count: usize,
) -> Option<MouseAction> {
    resolve_scrolling_up_for_direction(scroll_direction).map(|is_scrolling_up| {
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up,
            scroll_line_count,
        }
    })
}

/// `Some(true)` for a wheel up, `Some(false)` for a wheel down, `None` for a
/// horizontal tick.
fn resolve_scrolling_up_for_direction(scroll_direction: ScrollDirection) -> Option<bool> {
    match scroll_direction {
        ScrollDirection::Up => Some(true),
        ScrollDirection::Down => Some(false),
        ScrollDirection::Left | ScrollDirection::Right => None,
    }
}

/// The command-level name for the clipboard the viewer's `copy.clipboard`
/// setting picks.
fn resolve_clipboard_target(
    clipboard_backend: koshi_config::types::ClipboardBackend,
) -> CopyTarget {
    match clipboard_backend {
        koshi_config::types::ClipboardBackend::Osc52 => CopyTarget::Osc52,
    }
}

/// Cells the pointer at `to` has moved from `from` toward the grabbed `side`,
/// signed for [`Command::ResizePane`]: positive grows the pane (its border moves
/// outward), negative shrinks it. Left/right borders read the x axis, up/down
/// borders read the y axis; motion on the other axis is ignored.
fn compute_resize_cell_delta(
    border_side: Direction,
    start_position: Point,
    end_position: Point,
) -> i16 {
    let outward_cell_delta = match border_side {
        Direction::Right => i32::from(end_position.column) - i32::from(start_position.column),
        Direction::Left => i32::from(start_position.column) - i32::from(end_position.column),
        Direction::Down => i32::from(end_position.row) - i32::from(start_position.row),
        Direction::Up => i32::from(start_position.row) - i32::from(end_position.row),
    };
    outward_cell_delta.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The cell `from` reaches when `n` cells of a border move asked for in
/// direction `resize_step` are accepted. The inverse of [`compute_resize_cell_delta`]: a positive
/// `step` grows the pane, which walks a left or up border toward zero and a
/// right or down border away from it. Left/right borders move along x, up/down
/// borders along y.
fn advance_resize_anchor(
    border_side: Direction,
    start_position: Point,
    resize_step: i16,
    accepted_cell_count: u16,
) -> Point {
    let moved_cell_count = i32::from(resize_step) * i32::from(accepted_cell_count);
    match border_side {
        Direction::Right => Point {
            column: shift_cell_coordinate(start_position.column, moved_cell_count),
            ..start_position
        },
        Direction::Left => Point {
            column: shift_cell_coordinate(start_position.column, -moved_cell_count),
            ..start_position
        },
        Direction::Down => Point {
            row: shift_cell_coordinate(start_position.row, moved_cell_count),
            ..start_position
        },
        Direction::Up => Point {
            row: shift_cell_coordinate(start_position.row, -moved_cell_count),
            ..start_position
        },
    }
}

/// `coord` moved `by` cells, saturating at both ends of the cell range, so a
/// border at a viewport edge cannot wrap.
fn shift_cell_coordinate(coordinate_value: u16, cell_delta: i32) -> u16 {
    (i32::from(coordinate_value) + cell_delta).clamp(0, i32::from(u16::MAX)) as u16
}

/// `mouse_kind` with its button replaced by `mouse_button`. Only a drag or release carries a
/// button koshi re-stamps from the capture; other kinds are returned unchanged.
fn replace_mouse_button(mouse_kind: MouseKind, mouse_button: MouseButton) -> MouseKind {
    match mouse_kind {
        MouseKind::Drag(_) => MouseKind::Drag(mouse_button),
        MouseKind::Release(_) => MouseKind::Release(mouse_button),
        other => other,
    }
}
