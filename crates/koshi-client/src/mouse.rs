//! What a mouse event means, decided by the viewer that received it.
//!
//! A viewer holds its own `mouse` and `copy` settings — how many lines one
//! wheel step scrolls, what the wheel does over a plain pane, whether a border
//! can be dragged, whether releasing a highlight copies it — so two viewers of
//! one session can answer the same event differently. It also holds a
//! [`MouseFrame`] of the frame it last painted, which says where every surface
//! sits, which line each pane's top visible row is, which mouse modes each
//! pane's program had, and the viewer chrome it was painted with. That is
//! everything a mouse event needs, so the viewer decides and the session only
//! executes.
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
//! send alternate-scroll arrows, move a pane border, run a [`Command`], or
//! move this client's view of a floating pane. The binary's event loop passes
//! each one to the session in the order it is given.
//!
//! The gesture state lives here too: the run of clicks, the one koshi gesture
//! the held left button drives (a selection, border, tab-strip or grab-handle
//! drag), the pane a held button is captured to, the release owed to a program
//! whose capture koshi ended, and the floating pane move this viewer sent and
//! draws ahead of the session. While floating pane move mode is on, it owns
//! every mouse event, and a left drag of an unpinned floating pane moves it.
//! While dock/undock mode is on, it owns every mouse event
//! (`Client::handle_placement_mouse`): a left drag floats a tiled pane, or
//! docks or moves the selected floating pane.
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

use ratatui::layout::{Position, Rect as RatatuiRect};

use koshi_config::types::WheelScroll;
use koshi_core::command::{
    ClearSelectionArgs, ClosePaneArgs, Command, CopyArgs, FocusPaneArgs, FocusTabArgs, FocusTarget,
    GridPosition, MoveFloatingPaneArgs, PanePlacementAnchor, Selection, SelectionKind,
    SetPaneMinimizedArgs, SetPanePinnedArgs, SetSelectionArgs, TabTarget, TiledPlacementTarget,
    VisualCommand,
};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, TabId};
use koshi_core::key::BindingModifierFlags;
use koshi_core::mouse::{
    is_mouse_kind_reported, MouseButton, MouseInput, MouseKind, MouseTracking, ScrollDirection,
};
use koshi_layout::placement::PlacementDestinations;
use koshi_renderer::snapshot::{
    find_shown_placement_handle_hover, FloatingPaneMoveDraft, FrameLayout, MouseFrame, MousePane,
    PlacementHandleHover, TabSnapshot, ViewerChrome,
};
use koshi_renderer::{
    compute_clamped_pane_cell, compute_content_rect, compute_pane_area,
    compute_tiled_pane_top_border, find_first_visible_tab_index, find_pane_content_rect,
    resolve_hit_region, HitRegion,
};

use crate::input::is_same_tab_placement_noop;
use crate::terminal::project_core_rect;
use crate::{
    Client, DockUndockMode, FloatingPaneMoveSelection, PaneMoveMode, PlacementInputAction,
    PlacementModeLifetime,
};

#[cfg(test)]
pub(crate) mod tests;

/// How long after a press a second press still counts as a double click, and a
/// third as a triple.
const DOUBLE_CLICK_THRESHOLD_DURATION: Duration = Duration::from_millis(400);

/// How often the view scrolls while a selection drag is held past a pane's top
/// or bottom edge.
const SELECTION_SCROLL_INTERVAL_DURATION: Duration = Duration::from_millis(15);

/// Lines the view scrolls per firing while a drag is held past an edge.
const SELECTION_SCROLL_LINE_COUNT: usize = 1;

/// Cells of horizontal drag that scroll the tab strip by one tab.
pub const TABLINE_DRAG_CELL_COUNT: i32 = 6;

/// What the viewer decided one wheel tick means: where the pointer is, and what
/// the session must do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelDecision {
    /// The pane whose content is under the pointer, or `None` over any other
    /// region. The viewer marks it hovered, and the renderer draws it in the
    /// hover color while it is not focused.
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
    /// Move this client's scrollback view of `pane_id` by `scroll_line_count`, up into history
    /// or back down toward live output. The session clamps the movement to the
    /// pane's retained history.
    Scroll {
        /// The pane whose view moves.
        pane_id: PaneId,
        /// Up into history, or down toward live output.
        is_scrolling_up: bool,
        /// Lines to move, from this viewer's `mouse.scroll_line_count`.
        scroll_line_count: usize,
    },
    /// Hand the event to the program in `pane_id` as a mouse report at the
    /// cell `pane_local_cell`. The session encodes it from that pane's live
    /// tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane_id: PaneId,
        /// What the mouse did.
        mouse_kind: MouseKind,
        /// The modifier keys held during the event.
        modifier_flags: BindingModifierFlags,
        /// The cell in the pane's content the event names, `(0, 0)` at its
        /// top-left content cell.
        pane_local_cell: Point,
    },
    /// Send `arrow_count` cursor arrow keys to `pane_id` — the alternate-scroll (`?1007`)
    /// translation of a wheel tick on the alternate screen.
    AlternateScrollArrows {
        /// The pane whose program receives the arrows.
        pane_id: PaneId,
        /// Up-arrows, or down-arrows.
        is_scrolling_up: bool,
        /// How many, from this viewer's `mouse.scroll_line_count`.
        arrow_count: usize,
    },
    /// Run `command` through the session's command door, attributed to this
    /// client's mouse. Focus and every selection change travel this way, and the
    /// session validates them as it validates a command typed at the CLI.
    Command(Box<Command>),
    /// Move this client's view of a floating pane under `command_id`. The
    /// session names `command_id` in a `PlacementCommandRejected` when it
    /// refuses the move.
    MoveFloatingPane {
        /// The id the move command carries.
        command_id: CommandId,
        /// The floating pane and its new top-left cell.
        command_args: MoveFloatingPaneArgs,
    },
    /// Move `pane_id`'s `border_side` border `requested_cell_count` cells in the
    /// direction `resize_step` names — `1` outward (the pane grows), `-1` inward.
    ///
    /// The session moves the border up to `requested_cell_count` cells, as far
    /// as the panes' minimum sizes and the pane area allow, then reports how
    /// many cells the pane's size changed; the viewer advances its drag anchor
    /// by exactly that many, so a pointer pushed past a pane's minimum size
    /// leaves the border at that limit and a reverse drag moves it the instant
    /// the pointer crosses back.
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
/// while the pointer is held past an edge, and which screen the pane was showing
/// when the drag started.
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
    /// When the view must next scroll while the pointer is held past the pane's
    /// top or bottom edge; `None` whenever the pointer is inside.
    next_scroll_time: Option<Instant>,
    /// Whether the pane was showing the alternate screen when the press landed.
    /// The anchor names a line of that screen's text, so a frame reporting the
    /// other screen ends the drag.
    is_on_alternate_screen: bool,
}

/// A pane-border drag under way: the pane whose border was grabbed, which side
/// it is, the second side of a grabbed corner, and the cell the last *accepted*
/// resize tracked to. The tracked cell advances only over the cells a resize
/// was accepted for, so pushing the pointer past a pane's minimum size leaves
/// the border at that limit and a reverse drag moves it the instant the
/// pointer crosses back. A left or right side tracks the cell's column, and an
/// up or down side tracks its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeDrag {
    /// The pane whose border is being dragged.
    pane_id: PaneId,
    /// Which of the pane's borders was grabbed.
    border_side: Direction,
    /// The `Up` or `Down` border a corner press also moves, beside a `Left` or
    /// `Right` `border_side`. `None` when the press moves one border.
    corner_horizontal_side: Option<Direction>,
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

/// A floating pane's grab handle held by the left button: the pane, the
/// top-left cell this viewer draws it at, and where the press found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingPaneHandleDrag {
    /// The floating pane being moved.
    pane_id: PaneId,
    /// The top-left cell the viewer draws the pane at, counted from its
    /// pane-area origin. It may lie past the pane area, and the frame draws
    /// the pane moved inside it.
    top_left_cell: Point,
    /// The screen cell the press landed on.
    press_position: Point,
    /// The top-left cell the pane was drawn at when the press landed.
    press_top_left_cell: Point,
}

/// A floating pane move that a grab-handle release, or Enter in floating pane
/// move mode, sent under `command_id`, drawn at `top_left_cell` until an
/// applied frame names it, the session rejects it, or the session drops
/// events. While one waits, a press on a grab handle starts no drag, and the
/// arrow keys of floating pane move mode move nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentFloatingPaneMove {
    /// The floating pane the move names.
    pane_id: PaneId,
    /// The top-left cell the move sent, counted from the viewer's pane-area
    /// origin.
    top_left_cell: Point,
    /// The id the move command carries.
    command_id: CommandId,
}

/// The one koshi gesture the held left button drives, from the press that
/// began it until its release. A left press ends any gesture still recorded
/// before it acts: the highlight a selection made stands and is not copied,
/// and a grab-handle drag sends no move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeftButtonGesture {
    /// Highlighting text in a pane.
    Selection(SelectionDrag),
    /// Moving a pane border.
    BorderResize(ResizeDrag),
    /// Scrolling the tab strip by dragging it.
    TablinePeek(TablineDrag),
    /// Moving a floating pane by its grab handle.
    FloatingPaneMove(FloatingPaneHandleDrag),
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

/// The pane and button held by a forwarded mouse gesture, and the cell its
/// last forwarded report named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseCapture {
    /// The pane receiving the gesture.
    pane_id: PaneId,
    /// The button that began the gesture.
    button: MouseButton,
    /// The cell in the pane's content that the last forwarded press or drag
    /// named, `(0, 0)` at its top-left content cell.
    last_pane_local_cell: Point,
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
    /// The pane this viewer's pointer is over, the placement handle it shows or
    /// marks, its input mode, its pane placement preview, where its tab
    /// strip is scrolled, whether it holds a link to its session, and where it
    /// draws a floating pane it moves, for the frame it is about to paint, and
    /// for the tab-strip steps and the grab-handle release that read it before
    /// that paint. While the pane placement preview shows, no pane is hovered.
    /// The placement handle hover is the pointer's hover while a left press on
    /// that handle starts a drag of its pane, and `None` otherwise
    /// ([`find_shown_placement_handle_hover`]). A floating pane move waits
    /// while the one a grab-handle release sent, or the one floating pane move
    /// mode sent, has no answer from the session.
    ///
    /// The floating pane move draft is the grab-handle drag under way, else
    /// the cell the arrow keys of floating pane move mode moved the selected
    /// pane to, else the floating pane move this viewer sent.
    ///
    /// A peek made on a tab other than `active_tab_id` is not applied, so a tab
    /// switch reveals the new tab. The peek is thrown away outright the moment
    /// the viewer sees a frame on another tab
    /// ([`note_active_tab`](Self::note_active_tab)), so switching back does not
    /// bring it out again.
    #[must_use]
    pub fn build_viewer_chrome(&self, active_tab_id: TabId) -> ViewerChrome {
        let is_pane_placement_visible = self.is_pane_placement_visible();
        let active_input_mode = self.get_active_input_mode();
        ViewerChrome {
            hovered_pane_id: if is_pane_placement_visible {
                None
            } else {
                self.hovered_pane_id
            },
            placement_handle_hover: find_shown_placement_handle_hover(
                self.placement_handle_hover,
                active_input_mode,
                self.link_status,
                is_pane_placement_visible,
                self.is_dock_undock_command_pending(),
                self.sent_floating_pane_move.is_some() || self.is_floating_pane_move_pending(),
            ),
            active_input_mode: Some(active_input_mode),
            is_pane_placement_visible,
            placement_source_pane_id: is_pane_placement_visible
                .then(|| self.get_placement_source_pane_id())
                .flatten(),
            tabline_offset: self
                .tabline_peek
                .filter(|tabline_peek| tabline_peek.active_tab_id == active_tab_id)
                .map(|tabline_peek| tabline_peek.first_visible_tab_index),
            link_status: self.link_status,
            floating_pane_move_draft: match (
                self.left_button_gesture,
                self.pane_move_mode,
                self.sent_floating_pane_move,
            ) {
                (Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)), _, _) => {
                    Some(FloatingPaneMoveDraft {
                        pane_id: floating_pane_handle_drag.pane_id,
                        top_left_cell: floating_pane_handle_drag.top_left_cell,
                    })
                }
                (
                    _,
                    Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                        selected_pane_id,
                        draft_top_left_cell: Some(draft_top_left_cell),
                        ..
                    })),
                    _,
                ) => Some(FloatingPaneMoveDraft {
                    pane_id: selected_pane_id,
                    top_left_cell: draft_top_left_cell,
                }),
                (_, _, Some(sent_floating_pane_move)) => Some(FloatingPaneMoveDraft {
                    pane_id: sent_floating_pane_move.pane_id,
                    top_left_cell: sent_floating_pane_move.top_left_cell,
                }),
                (_, _, None) => None,
            },
            floating_pane_tray: self.floating_pane_tray,
        }
    }

    /// Take in the tab a frame this viewer just looked at is showing, and throw
    /// away a tab-strip peek made on any other tab.
    ///
    /// The peek is the viewer's own and follows the frames it sees. A switch
    /// back starts fresh — peek from tab 3 while tab 0 is active, switch to tab 1, switch
    /// back to tab 0, and the strip starts at tab 0.
    pub fn note_active_tab(&mut self, active_tab_id: TabId) {
        self.tabline_peek = self
            .tabline_peek
            .filter(|tabline_peek| tabline_peek.active_tab_id == active_tab_id);
    }

    /// Route `mouse_input` while pane placement owns the mouse. Returns `None`
    /// when the event is not placement input and takes the normal mouse path.
    ///
    /// - While dock/undock mode is on, that mode answers every event
    ///   ([`handle_dock_undock_mouse`](Self::handle_dock_undock_mouse)).
    /// - While floating pane move mode is on, every event returns `None`.
    /// - While a submitted placement waits for the session, every event is
    ///   consumed and the placement drag ends.
    /// - With placement mode off, an unshifted left press on a tiled pane's
    ///   placement handle opens placement mode for that pane until the drag
    ///   ends. A press on a floating pane's grab handle, a Shift press, and
    ///   every other event return `None`.
    /// - With placement mode on, a left press on a tab previews that tab. A
    ///   left press on pane content, a placement handle, or a stack header
    ///   starts a drag, and picks that pane as the source unless it already is
    ///   the source. A pane picked in the session's active tab also takes
    ///   focus: a press on the header of collapsed `pane-123` returns a
    ///   `FocusPane` of `pane-123`, which expands it in its stack. A pane
    ///   picked while another tab is previewed keeps focus where it is.
    /// - A left drag selects the target under the pointer: a swap, or an
    ///   insertion while Shift was held at the press. A left drag onto a scroll
    ///   arrow also steps the tab strip one tab, as a wheel tick over the tab
    ///   strip does. A left release after the pointer moved submits the target
    ///   under it. A release that submits nothing cancels placement mode that
    ///   lasts until its drag ends. Every other event is consumed.
    pub(crate) fn handle_placement_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Option<PlacementInputAction> {
        if self.dock_undock_mode.is_some() {
            return Some(self.handle_dock_undock_mouse(mouse_input, mouse_frame, current_time));
        }
        if self.is_floating_pane_move_mode_on() {
            return None;
        }
        if self.is_placement_confirmation_pending() {
            self.end_placement_drag();
            return Some(PlacementInputAction::Consumed);
        }
        let frame_layout = mouse_frame.build_frame_layout();
        if !self.is_placement_mode_active() {
            if mouse_input.mouse_kind != MouseKind::Press(MouseButton::Left)
                || mouse_input
                    .modifier_flags
                    .has_all_modifiers(BindingModifierFlags::SHIFT)
            {
                return None;
            }
            let HitRegion::PlacementHandle { pane_id } =
                resolve_hit_region(frame_layout, mouse_input.position)
            else {
                return None;
            };
            if frame_layout
                .find_drawn_floating_pane_slot(pane_id)
                .is_some()
            {
                return None;
            }
            let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
            if !self.begin_mouse_placement_mode(pane_id, active_tab_id) {
                return Some(PlacementInputAction::Consumed);
            }
            self.begin_placement_drag(mouse_input.position, false);
            return Some(PlacementInputAction::FocusPane { pane_id });
        }
        let hit_region = resolve_hit_region(frame_layout, mouse_input.position);
        self.update_placement_hovers(mouse_frame, mouse_input, hit_region, current_time);
        let placement_action = match mouse_input.mouse_kind {
            MouseKind::Press(MouseButton::Left) => match hit_region {
                HitRegion::Tab { tab_id } => {
                    self.select_placement_destination_tab(tab_id);
                    PlacementInputAction::Consumed
                }
                HitRegion::PlacementHandle { pane_id }
                | HitRegion::PaneContent { pane_id }
                | HitRegion::StackHeader { pane_id } => {
                    let displayed_tab_id = mouse_frame.client_snapshot.active_tab_id;
                    let is_displayed_tab_active = self.active_tab_id == Some(displayed_tab_id);
                    let is_source_pane_selected =
                        self.select_placement_source_pane(pane_id, displayed_tab_id);
                    self.begin_placement_drag(
                        mouse_input.position,
                        mouse_input
                            .modifier_flags
                            .has_all_modifiers(BindingModifierFlags::SHIFT),
                    );
                    if is_source_pane_selected && is_displayed_tab_active {
                        PlacementInputAction::FocusPane { pane_id }
                    } else {
                        PlacementInputAction::Consumed
                    }
                }
                HitRegion::PaneBorder { .. }
                | HitRegion::PaneTitlebar { .. }
                | HitRegion::PanePinToggle { .. }
                | HitRegion::PaneMinimizeButton { .. }
                | HitRegion::PaneCloseButton { .. }
                | HitRegion::Tabline
                | HitRegion::TablineScrollLeft
                | HitRegion::TablineScrollRight
                | HitRegion::FloatingPaneTrayButton
                | HitRegion::FloatingPaneTrayTile { .. }
                | HitRegion::FloatingPaneTrayToggleMinimizeAll
                | HitRegion::FloatingPaneTray
                | HitRegion::Statusline
                | HitRegion::None => PlacementInputAction::Consumed,
            },
            MouseKind::Drag(MouseButton::Left) => {
                match hit_region {
                    HitRegion::TablineScrollLeft => {
                        self.step_tabline_peek(mouse_frame, ScrollDirection::Left);
                    }
                    HitRegion::TablineScrollRight => {
                        self.step_tabline_peek(mouse_frame, ScrollDirection::Right);
                    }
                    _ => {}
                }
                if self.update_placement_drag_movement(mouse_input.position) {
                    if let Some(placement_target) =
                        self.find_placement_target_at(mouse_input.position, mouse_frame)
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
                if is_tab_strip_region(hit_region) {
                    self.step_tabline_peek(mouse_frame, scroll_direction);
                }
                PlacementInputAction::Consumed
            }
            MouseKind::Release(released_button) => {
                let should_submit = released_button == MouseButton::Left
                    && self.update_placement_drag_movement(mouse_input.position);
                if should_submit {
                    let placement_target =
                        self.find_placement_target_at(mouse_input.position, mouse_frame);
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
            _ => PlacementInputAction::Consumed,
        };
        Some(placement_action)
    }

    /// Answer `mouse_input` while dock/undock mode owns the mouse. The pane
    /// under the pointer follows the pointer, and no event reaches a pane's
    /// program.
    ///
    /// - A left press drops the left-button gesture under way. While the mode
    ///   waits for the undock or the dock it sent, it does nothing more.
    /// - A left press on a tab previews that tab while a floating pane is
    ///   selected.
    /// - A left press on a pane, or on a collapsed stack member's header,
    ///   begins a pane drag
    ///   ([`begin_dock_undock_pane_drag`](Self::begin_dock_undock_pane_drag)).
    /// - A left drag of a tiled pane that moves the pointer off the press cell
    ///   sends the `PlacePane` that floats the pane where it is drawn, and the
    ///   pane becomes the selected pane
    ///   ([`submit_undock_command`](Self::submit_undock_command)). The drag
    ///   goes on.
    /// - A left drag of the selected floating pane moves it with the pointer,
    ///   unless this viewer pinned it, and selects the insertion under the
    ///   pointer, on the edge of the pane nearest it, as a Shift drag in pane
    ///   placement mode does. A left drag onto a scroll arrow steps the tab
    ///   strip one tab.
    /// - A left release of the selected floating pane over a target confirms
    ///   the placement that docks it there
    ///   ([`submit_placement_command`](Self::submit_placement_command)). A
    ///   release over no target, or over a target the session refuses in the
    ///   held preview, such as a tab with no room, clears the target and sends
    ///   one `MoveFloatingPane` to the cell the pane is drawn at
    ///   ([`submit_floating_pane_move`](Self::submit_floating_pane_move)), or
    ///   nothing for a pinned pane. A release of a tiled pane's drag sends
    ///   nothing. A frame that floats the dragged tiled pane makes it the
    ///   selected floating pane, and the drag goes on from where that frame
    ///   draws it
    ///   ([`update_dock_undock_selection`](Self::update_dock_undock_selection)).
    /// - A wheel tick over the tab strip steps it one tab. Every other event
    ///   changes nothing.
    ///
    /// Every answer is `Consumed` unless it names a command or a confirmation
    /// above.
    fn handle_dock_undock_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> PlacementInputAction {
        let hit_region = resolve_hit_region(mouse_frame.build_frame_layout(), mouse_input.position);
        self.update_placement_hovers(mouse_frame, mouse_input, hit_region, current_time);
        match mouse_input.mouse_kind {
            MouseKind::Press(MouseButton::Left) => {
                self.left_button_gesture = None;
                if self.is_dock_undock_command_pending() {
                    self.end_placement_drag();
                    return PlacementInputAction::Consumed;
                }
                match hit_region {
                    HitRegion::Tab { tab_id } => {
                        self.select_placement_destination_tab(tab_id);
                        PlacementInputAction::Consumed
                    }
                    HitRegion::PaneContent { pane_id }
                    | HitRegion::PlacementHandle { pane_id }
                    | HitRegion::StackHeader { pane_id }
                    | HitRegion::PaneTitlebar { pane_id } => {
                        self.begin_dock_undock_pane_drag(
                            pane_id,
                            mouse_input.position,
                            mouse_frame,
                        );
                        PlacementInputAction::Consumed
                    }
                    _ => PlacementInputAction::Consumed,
                }
            }
            MouseKind::Drag(MouseButton::Left) => {
                match hit_region {
                    HitRegion::TablineScrollLeft => {
                        self.step_tabline_peek(mouse_frame, ScrollDirection::Left);
                    }
                    HitRegion::TablineScrollRight => {
                        self.step_tabline_peek(mouse_frame, ScrollDirection::Right);
                    }
                    _ => {}
                }
                let Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) =
                    self.left_button_gesture
                else {
                    return PlacementInputAction::Consumed;
                };
                self.drag_floating_pane_to(floating_pane_handle_drag, mouse_input.position);
                let pane_id = floating_pane_handle_drag.pane_id;
                if self.get_placement_source_pane_id() == Some(pane_id) {
                    if self.update_placement_drag_movement(mouse_input.position) {
                        let placement_target =
                            self.find_placement_target_at(mouse_input.position, mouse_frame);
                        self.set_mouse_placement_target(placement_target);
                    }
                    return PlacementInputAction::Consumed;
                }
                let is_tiled_pane_dragged = mouse_input.position
                    != floating_pane_handle_drag.press_position
                    && !mouse_frame.session_snapshot.is_floating_pane(pane_id);
                if !is_tiled_pane_dragged || self.is_dock_undock_command_pending() {
                    return PlacementInputAction::Consumed;
                }
                self.cancel_placement_mode();
                self.submit_undock_command(pane_id)
            }
            MouseKind::Release(MouseButton::Left) => {
                let Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) =
                    self.left_button_gesture
                else {
                    self.end_placement_drag();
                    return PlacementInputAction::Consumed;
                };
                if self.get_placement_source_pane_id() != Some(floating_pane_handle_drag.pane_id) {
                    self.left_button_gesture = None;
                    return PlacementInputAction::Consumed;
                }
                let placement_target = if self.update_placement_drag_movement(mouse_input.position)
                {
                    self.find_placement_target_at(mouse_input.position, mouse_frame)
                } else {
                    None
                };
                self.end_placement_drag();
                if placement_target.is_some() {
                    self.set_mouse_placement_target(placement_target);
                    let placement_action = self
                        .submit_placement_command()
                        .unwrap_or(PlacementInputAction::Consumed);
                    if self.is_placement_confirmation_pending() {
                        self.left_button_gesture = None;
                        return placement_action;
                    }
                }
                self.clear_mouse_placement_target();
                self.submit_floating_pane_move(
                    floating_pane_handle_drag,
                    mouse_input.position,
                    mouse_frame,
                )
                .map_or(
                    PlacementInputAction::Consumed,
                    |(command_id, command_args)| PlacementInputAction::SubmitCommand {
                        command_id,
                        command: Command::MoveFloatingPane(command_args),
                    },
                )
            }
            MouseKind::Scroll(scroll_direction) => {
                if is_tab_strip_region(hit_region) {
                    self.step_tabline_peek(mouse_frame, scroll_direction);
                }
                PlacementInputAction::Consumed
            }
            _ => PlacementInputAction::Consumed,
        }
    }

    /// Begin the dock/undock mode drag of `pane_id`, which `mouse_frame`
    /// draws, from the left press at `press_position`: a grab-handle drag that
    /// starts where the frame draws the pane, counted from the pane-area
    /// origin. A tiled pane's drawn rect is its pane box, and a collapsed
    /// stack member's is its header strip.
    ///
    /// - The selected floating pane, drawn floating or in the dock preview,
    ///   also begins pane placement mode's insertion drag, and its target
    ///   clears.
    /// - Another floating pane becomes the selected pane: pane placement mode
    ///   opens for it with no target, and previews the frame's active tab.
    /// - A tiled pane starts the drag only. Its first move floats the pane.
    ///
    /// Changes nothing while a floating pane move this viewer sent waits for
    /// the session and the pane is floating, and for a pane the frame draws
    /// nowhere.
    fn begin_dock_undock_pane_drag(
        &mut self,
        pane_id: PaneId,
        press_position: Point,
        mouse_frame: &MouseFrame,
    ) {
        let pane_area = compute_mouse_frame_pane_area(mouse_frame);
        let drawn_floating_pane_slot = mouse_frame
            .build_frame_layout()
            .find_drawn_floating_pane_slot(pane_id);
        let drawn_top_left_cell = match drawn_floating_pane_slot {
            Some(floating_pane_slot) => floating_pane_slot.outer_rect.origin,
            None => {
                let active_tab_snapshot = &mouse_frame.session_snapshot.active_tab_snapshot;
                let Some(pane_slot) = active_tab_snapshot.pane_slots.iter().find(|pane_slot| {
                    pane_slot.pane_id == pane_id && !pane_slot.outer_rect.is_empty()
                }) else {
                    return;
                };
                let tab_origin = Size {
                    column_count: pane_area.width,
                    row_count: pane_area.height,
                }
                .compute_centered_origin(active_tab_snapshot.tab_size);
                let pane_slot_origin = pane_slot.outer_rect.origin;
                Point {
                    column: pane_slot_origin.column.saturating_add(tab_origin.column),
                    row: pane_slot_origin.row.saturating_add(tab_origin.row),
                }
            }
        };
        let is_pane_floating = mouse_frame.session_snapshot.is_floating_pane(pane_id);
        if is_pane_floating && self.sent_floating_pane_move.is_some() {
            return;
        }
        let is_selected_pane = self.get_placement_source_pane_id() == Some(pane_id);
        if is_pane_floating && !is_selected_pane {
            self.cancel_placement_mode();
            let destination_tab_id = mouse_frame.client_snapshot.active_tab_id;
            self.dock_undock_mode = Some(DockUndockMode::default());
            self.open_placement_mode(
                pane_id,
                None,
                destination_tab_id,
                PlacementModeLifetime::UntilCancelled,
            );
        }
        self.begin_floating_pane_handle_drag(pane_id, drawn_top_left_cell, press_position);
        if is_pane_floating {
            self.clear_mouse_placement_target();
            self.begin_placement_drag(press_position, true);
        }
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
    fn set_mouse_placement_target(&mut self, placement_target: Option<TiledPlacementTarget>) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        let is_placement_target_noop = placement_target.as_ref().is_some_and(|placement_target| {
            self.get_placement_snapshot()
                .is_some_and(|placement_snapshot| {
                    is_same_tab_placement_noop(placement_snapshot, placement_target)
                })
        });
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        let is_target_source_pane = Some(placement_mode.destination_tab_id)
            == placement_mode.source_tab_id
            && placement_target.as_ref().is_some_and(|placement_target| {
                matches!(
                    placement_target,
                    TiledPlacementTarget::Swap { target_pane_id }
                        | TiledPlacementTarget::Split {
                            anchor: PanePlacementAnchor::Pane(target_pane_id),
                            ..
                        } if *target_pane_id == placement_mode.source_pane_id
                )
            });
        if is_target_source_pane || is_placement_target_noop {
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
        mouse_frame: &MouseFrame,
    ) -> Option<TiledPlacementTarget> {
        let placement_mode = self.placement_state.placement_mode.as_ref()?;
        let placement_snapshot = self.get_placement_snapshot()?;
        let tab_snapshot = &placement_snapshot
            .find_displayed_tab_snapshot()?
            .tab_snapshot;
        if tab_snapshot.is_every_pane_suppressed {
            return None;
        }
        let destination_layout_rect = compute_content_rect(
            compute_mouse_frame_pane_area(mouse_frame),
            tab_snapshot.tab_size,
        );
        if !destination_layout_rect.contains(Position::new(screen_point.column, screen_point.row)) {
            return None;
        }
        if tab_snapshot.tab_size.column_count == 0 || tab_snapshot.tab_size.row_count == 0 {
            return None;
        }
        let hovered_pane_id =
            Self::find_placement_pane_id_at(screen_point, destination_layout_rect, tab_snapshot);
        if hovered_pane_id == Some(placement_mode.source_pane_id) {
            return None;
        }
        let placement_drag = self.placement_state.placement_drag?;
        let destination_tab_id = placement_mode.destination_tab_id;
        let placement_destinations = placement_snapshot.list_placement_destinations()?;
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
                    tab_snapshot.tab_size,
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
                        tab_snapshot.tab_size,
                        destination_layout_rect,
                    )
                })?;
            return Some(TiledPlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Pane(target_pane_id),
                direction: compute_nearest_edge_direction(screen_point, target_rect),
            });
        }

        let target_pane_id = hovered_pane_id?;
        let target_pane_slot = placement_destinations
            .target_pane_slots
            .iter()
            .find(|target_pane_slot| target_pane_slot.pane_id == target_pane_id)?;
        Some(TiledPlacementTarget::Swap {
            target_pane_id: target_pane_slot.pane_id,
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
        placement_tab_snapshot: &TabSnapshot,
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

    /// Decide what `mouse_input` means against `mouse_frame`, the last frame
    /// this viewer painted, and return everything the session must do about it,
    /// in order. `mouse_frame` is the frame last passed to
    /// [`drop_gestures_the_frame_ended`](Self::drop_gestures_the_frame_ended).
    /// The hovered pane is cleared first when `mouse_frame` no longer draws
    /// it. The placement handle hover is cleared first unless it names a
    /// floating pane `mouse_frame` draws, and a pointer motion sets a tiled
    /// pane's again.
    ///
    /// A **wheel tick** is answered by [`handle_mouse_wheel`](Self::handle_mouse_wheel).
    /// The rest is routed by what the press began: a border drag resizes, a
    /// floating pane's grab-handle drag moves that pane in this viewer's own
    /// view, a tab-strip drag scrolls the strip, a content drag highlights, and
    /// anything else is the program's. A **left press** acts on the region it
    /// landed on — a tab focuses that tab, a scroll arrow steps the strip one tab, the
    /// floating pane tray button opens the tray, a
    /// stack header focuses that pane, pane content focuses the pane or begins
    /// a highlight, a border begins a resize, the bare tab strip begins a
    /// peek-drag. On a floating pane, a press on the grab handle begins a move,
    /// a press on the border, the grab handle or the titlebar also focuses and
    /// raises the pane, and the pin toggle, minimize and close buttons act
    /// without focusing it. A **left release** ends whichever drag was under
    /// way, and a grab-handle release sends the move. A press, drag or release
    /// of another button goes to the program, and a left drag under way goes
    /// on. A buttonless **move** updates which pane is hovered and which
    /// placement handle shows, and reaches the program if it asked for moves.
    ///
    /// A **press** first ends every gesture its button still holds: a left
    /// press drops koshi's left-button gesture with no copy and no move sent,
    /// and a press of the button a capture holds ends that capture. A capture
    /// ended without its release puts one release of its button, at its last
    /// cell, ahead of every other action.
    ///
    /// While floating pane move mode is on, the mode answers every event other
    /// than those first steps, and no event reaches a pane's program.
    ///
    /// A left press on the content of an already-focused plain shell at the
    /// pane's third visible row, with the pane's top row on line 940, arms a
    /// character drag anchored at line 942 and dispatches nothing yet.
    pub fn handle_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        self.note_active_tab(mouse_frame.client_snapshot.active_tab_id);
        self.hovered_pane_id = self
            .hovered_pane_id
            .filter(|&pane_id| is_pane_drawn(mouse_frame, pane_id));
        // Keeps only the grab handle hover of a floating pane the frame draws,
        // with the pin state the frame draws. A pointer motion sets a tiled
        // pane's placement handle hover again in `update_pointer_hover`.
        self.placement_handle_hover = match self.placement_handle_hover {
            Some(PlacementHandleHover::FloatingHandle { pane_id, .. }) => mouse_frame
                .build_frame_layout()
                .find_drawn_floating_pane_slot(pane_id)
                .map(|floating_pane_slot| PlacementHandleHover::FloatingHandle {
                    pane_id,
                    is_pinned: floating_pane_slot.is_pinned,
                }),
            _ => None,
        };
        if let MouseKind::Press(pressed_button) = mouse_input.mouse_kind {
            if pressed_button == MouseButton::Left {
                self.left_button_gesture = None;
            }
            if self
                .mouse_capture
                .is_some_and(|mouse_capture| mouse_capture.button == pressed_button)
            {
                self.end_mouse_capture();
            }
        }
        let mut mouse_actions = if self.is_floating_pane_move_mode_on() {
            self.handle_floating_pane_move_mouse(mouse_input, mouse_frame)
        } else {
            self.handle_mouse_event_on_frame(mouse_input, mouse_frame, current_time)
        };
        if let Some(capture_release) = self.take_mouse_capture_release() {
            mouse_actions.insert(0, capture_release);
        }
        mouse_actions
    }

    /// Answer `mouse_input` outside floating pane move mode, by what the press
    /// began, as [`handle_mouse`](Self::handle_mouse) describes.
    fn handle_mouse_event_on_frame(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        match mouse_input.mouse_kind {
            MouseKind::Scroll(_) => match self.handle_mouse_wheel(mouse_input, mouse_frame) {
                Some(wheel_decision) => {
                    self.hovered_pane_id = wheel_decision.hovered_pane_id;
                    wheel_decision.mouse_action.into_iter().collect()
                }
                None => Vec::new(),
            },
            MouseKind::Press(MouseButton::Left) => {
                self.handle_left_press(mouse_input, mouse_frame, current_time)
            }
            MouseKind::Drag(MouseButton::Left) => {
                self.handle_left_drag(mouse_input, mouse_frame, current_time)
            }
            MouseKind::Release(MouseButton::Left) => {
                self.release_mouse_gesture(mouse_input, mouse_frame)
            }
            MouseKind::Motion => {
                let hit_region =
                    resolve_hit_region(mouse_frame.build_frame_layout(), mouse_input.position);
                self.update_pointer_hover(mouse_frame, mouse_input, hit_region);
                self.forward_mouse_input(mouse_input, mouse_frame)
            }
            MouseKind::Press(_) | MouseKind::Drag(_) | MouseKind::Release(_) => {
                self.forward_mouse_input(mouse_input, mouse_frame)
            }
        }
    }

    /// Answer `mouse_input` while floating pane move mode owns the mouse. The
    /// pane under the pointer and the marked grab handle follow the pointer,
    /// and no event reaches a pane's program.
    ///
    /// - A left press on the content or the grab handle of a floating pane
    ///   this viewer draws unpinned selects that pane, starts a grab-handle
    ///   drag of it, and returns a `FocusPane` for it, whichever pane
    ///   `mouse_frame` focuses. A press on another pane drops the draft. The
    ///   press does nothing while a sent floating pane move waits for the
    ///   session, and while the mode waits for the move it sent
    ///   ([`is_floating_pane_move_pending`](Self::is_floating_pane_move_pending)).
    /// - A left drag moves that drag's draft
    ///   ([`drag_floating_pane_to`](Self::drag_floating_pane_to)).
    /// - A left release sends the move
    ///   ([`submit_floating_pane_move`](Self::submit_floating_pane_move)).
    ///   A release that sends a move drops the draft, and the mode waits for
    ///   that move
    ///   ([`record_sent_floating_pane_move`](Self::record_sent_floating_pane_move)).
    /// - Every other event returns nothing.
    fn handle_floating_pane_move_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let frame_layout = self.build_frame_layout_with_current_chrome(mouse_frame);
        let hit_region = resolve_hit_region(frame_layout, mouse_input.position);
        self.update_pointer_hover(mouse_frame, mouse_input, hit_region);
        match mouse_input.mouse_kind {
            MouseKind::Press(MouseButton::Left) => {
                let (HitRegion::PaneContent { pane_id } | HitRegion::PlacementHandle { pane_id }) =
                    hit_region
                else {
                    return Vec::new();
                };
                let Some(floating_pane_slot) = frame_layout.find_drawn_floating_pane_slot(pane_id)
                else {
                    return Vec::new();
                };
                if floating_pane_slot.is_pinned
                    || self.sent_floating_pane_move.is_some()
                    || self.is_floating_pane_move_pending()
                {
                    return Vec::new();
                }
                let is_selected_pane = matches!(
                    self.pane_move_mode,
                    Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection))
                        if floating_pane_move_selection.selected_pane_id == pane_id
                );
                if !is_selected_pane {
                    self.pane_move_mode =
                        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                            selected_pane_id: pane_id,
                            draft_top_left_cell: None,
                            pending_move_command_id: None,
                        }));
                }
                self.begin_floating_pane_handle_drag(
                    pane_id,
                    floating_pane_slot.outer_rect.origin,
                    mouse_input.position,
                );
                vec![build_focus_pane_action(self.client_id, pane_id)]
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) =
                    self.left_button_gesture
                {
                    self.drag_floating_pane_to(floating_pane_handle_drag, mouse_input.position);
                }
                Vec::new()
            }
            MouseKind::Release(MouseButton::Left) => {
                let Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) =
                    self.left_button_gesture
                else {
                    return Vec::new();
                };
                self.submit_floating_pane_move_as_mouse_actions(
                    floating_pane_handle_drag,
                    mouse_input.position,
                    mouse_frame,
                )
            }
            _ => Vec::new(),
        }
    }

    /// How long until a selection drag held past a pane's edge must scroll the
    /// view again, or `None` when no drag is held there. The event loop blocks
    /// no longer than this, and a pointer held still past the edge scrolls the
    /// view at each wakeup. While an input event this terminal's input thread
    /// read has not reached the loop (`InputReadClock::has_untaken_input`), the
    /// answer is `None` too: that event's arrival wakes the loop.
    #[must_use]
    pub fn compute_next_mouse_wakeup(&self, current_time: Instant) -> Option<Duration> {
        if self.input_read_clock.has_untaken_input() {
            return None;
        }
        self.find_selection_drag()
            .and_then(|selection_drag| selection_drag.next_scroll_time)
            .map(|next_scroll_time| next_scroll_time.saturating_duration_since(current_time))
    }

    /// The selection drag the held left button drives, or `None` when it
    /// drives another gesture or none.
    fn find_selection_drag(&self) -> Option<SelectionDrag> {
        match self.left_button_gesture {
            Some(LeftButtonGesture::Selection(selection_drag)) => Some(selection_drag),
            _ => None,
        }
    }

    /// Scroll a selection drag held past a pane's edge, if its next step is due
    /// at `current_time`.
    ///
    /// Each firing moves the view one line toward the pointer. The highlight is
    /// re-extended in [`note_scroll_applied`](Self::note_scroll_applied), once
    /// the session reports where the view landed.
    ///
    /// A step that is not due yet yields no actions and leaves the timer where
    /// it is. A pointer no longer outside the pane, and a pane the frame no
    /// longer carries, yield no actions and disarm; the next drag event arms it
    /// again.
    pub fn expire_mouse_scroll(
        &mut self,
        current_time: Instant,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(selection_drag) = self.find_selection_drag() else {
            return Vec::new();
        };
        if selection_drag
            .next_scroll_time
            .is_none_or(|next_scroll_time| next_scroll_time > current_time)
        {
            return Vec::new();
        }
        let scroll_direction = self.compute_edge_scroll_direction(
            mouse_frame,
            selection_drag.pane_id,
            selection_drag.pointer_position,
        );
        let view_top_row_index = find_mouse_pane(mouse_frame, selection_drag.pane_id)
            .map(|mouse_pane| mouse_pane.view_top_row_index);
        let (Some(scroll_direction), Some(view_top_row_index)) =
            (scroll_direction, view_top_row_index)
        else {
            // The pane moved out from under a pointer that is no longer outside
            // it; stop scrolling and let the next drag event drive.
            self.left_button_gesture = Some(LeftButtonGesture::Selection(SelectionDrag {
                next_scroll_time: None,
                ..selection_drag
            }));
            return Vec::new();
        };
        self.selection_scroll_origin_row_index = Some(view_top_row_index);
        self.left_button_gesture = Some(LeftButtonGesture::Selection(SelectionDrag {
            next_scroll_time: Some(current_time + SELECTION_SCROLL_INTERVAL_DURATION),
            ..selection_drag
        }));
        vec![MouseAction::Scroll {
            pane_id: selection_drag.pane_id,
            is_scrolling_up: scroll_direction < 0,
            scroll_line_count: SELECTION_SCROLL_LINE_COUNT,
        }]
    }

    /// Take in where the view landed after a scroll: `view_top_row_index` is the line `pane_id`'s
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
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(origin_row_index) = self.selection_scroll_origin_row_index.take() else {
            return Vec::new();
        };
        let Some(selection_drag) = self
            .find_selection_drag()
            .filter(|selection_drag| selection_drag.pane_id == pane_id)
        else {
            return Vec::new();
        };
        let Some(reported_view_top_row_index) =
            view_top_row_index.filter(|&view_top_row_index| view_top_row_index != origin_row_index)
        else {
            self.left_button_gesture = Some(LeftButtonGesture::Selection(SelectionDrag {
                next_scroll_time: None,
                ..selection_drag
            }));
            return Vec::new();
        };
        let Some(pane_local_cell) = compute_clamped_pane_cell(
            mouse_frame.build_frame_layout(),
            pane_id,
            selection_drag.pointer_position,
        ) else {
            return Vec::new();
        };
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: selection_drag.selection_kind,
                    anchor: selection_drag.anchor_grid_position,
                    cursor: GridPosition {
                        row_index: reported_view_top_row_index + u64::from(pane_local_cell.row),
                        column_index: pane_local_cell.column,
                    },
                },
            }),
        )))]
    }

    /// Drop the selection gesture under way, if any, leaving the highlight it
    /// made. Called when a key reaches the pane's program.
    pub fn end_mouse_selection(&mut self) {
        self.left_button_gesture.take_if(|left_button_gesture| {
            matches!(left_button_gesture, LeftButtonGesture::Selection(_))
        });
    }

    /// Drop every gesture a held button drives: koshi's left-button gesture,
    /// the placement drag, and the pane a held button was captured to, whose
    /// program is owed a release ([`end_mouse_capture`](Self::end_mouse_capture)).
    /// The highlight a selection drag already made, and a floating pane move
    /// already sent, stand.
    pub(crate) fn end_held_mouse_gestures(&mut self) {
        self.left_button_gesture = None;
        self.placement_state.placement_drag = None;
        self.end_mouse_capture();
    }

    /// End the capture before its button's release reaches the pane's
    /// program. That program is owed one release of the capture's button at
    /// the capture's last cell
    /// ([`take_mouse_capture_release`](Self::take_mouse_capture_release)).
    /// Does nothing when no capture is held.
    fn end_mouse_capture(&mut self) {
        if let Some(mouse_capture) = self.mouse_capture.take() {
            self.unreleased_mouse_capture = Some(mouse_capture);
        }
    }

    /// Take the release this viewer owes the program of a capture it ended:
    /// one release of the capture's button, at the capture's last cell, with
    /// no modifier held. `None` when no release is owed.
    ///
    /// A left capture in `pane-7` whose last drag named cell `(4, 2)`, ended
    /// by placement mode opening, gives a left release at `(4, 2)` to
    /// `pane-7`.
    pub(crate) fn take_mouse_capture_release(&mut self) -> Option<MouseAction> {
        let MouseCapture {
            pane_id,
            button,
            last_pane_local_cell,
        } = self.unreleased_mouse_capture.take()?;
        Some(MouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(button),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: last_pane_local_cell,
        })
    }

    /// Drop every gesture under way: the selection drag, the border drag, the
    /// tab-strip peek-drag, the floating pane grab-handle drag, the placement
    /// drag, the pane a held button was captured to, the floating pane move
    /// this viewer draws ahead of the session after it sent it, and the cell
    /// the arrow keys of floating pane move mode moved the selected pane to.
    /// No release is owed for the capture, and a release already owed is
    /// dropped. The highlight a selection drag already made stands. Called
    /// when a new link to the session replaces the old one.
    /// Placement mode that lasts until its drag ends is cancelled. Placement
    /// mode that lasts until Esc stays on and loses its unconfirmed target.
    /// The move chooser, floating pane move mode, and dock/undock mode stay
    /// on, and floating pane move mode keeps its selected pane. A wait for the
    /// move the mode sent ends at the next applied frame.
    ///
    /// The pane under the pointer, where the tab strip is peeked, and the line a
    /// pending edge-scroll was asked from are left as they are.
    pub fn end_mouse_gestures(&mut self) {
        self.end_held_mouse_gestures();
        self.unreleased_mouse_capture = None;
        self.sent_floating_pane_move = None;
        self.set_floating_pane_move_draft(None);
        if self.should_placement_mode_end_with_drag() {
            self.cancel_placement_mode();
        } else if !self.is_placement_confirmation_pending() {
            if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
                placement_mode.placement_target = None;
            }
        }
    }

    /// Decide what wheel tick `mouse_input` means against `mouse_frame`, the last frame this
    /// viewer painted. Returns `None` when `mouse_input` is not a wheel tick.
    ///
    /// Over the tab strip the tick steps the strip. Elsewhere it targets the
    /// pane whose content is under the pointer, the floating pane whose
    /// titlebar, border, grab handle or buttons are under the pointer, or this
    /// viewer's focused terminal pane when the pointer is over other chrome,
    /// and that pane is answered by precedence:
    ///
    /// 1. a highlight in the pane makes the tick scroll koshi's own scrollback;
    /// 2. else a program asking for the mouse gets the tick as a report;
    /// 3. else an alternate-screen program with `?1007` on gets arrow keys;
    /// 4. else this viewer's
    ///    [`mouse.wheel`](koshi_config::types::MouseConfig::wheel_scroll) decides —
    ///    scroll koshi's scrollback (the default), or do nothing.
    ///
    /// A wheel up over a plain shell with `scroll_line_count = 3` yields
    /// `Scroll { pane_id, is_scrolling_up: true, scroll_line_count: 3 }`; the
    /// same tick over a `vim` in normal tracking yields `Forward`.
    ///
    /// A tick over the tab strip moves this viewer's own peek and yields no
    /// action: nothing on the session changes.
    #[must_use]
    pub fn handle_mouse_wheel(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Option<WheelDecision> {
        let MouseKind::Scroll(scroll_direction) = mouse_input.mouse_kind else {
            return None;
        };
        let frame_layout = mouse_frame.build_frame_layout();
        let hit_region = resolve_hit_region(frame_layout, mouse_input.position);
        if is_tab_strip_region(hit_region) {
            self.step_tabline_peek(mouse_frame, scroll_direction);
            return Some(WheelDecision {
                hovered_pane_id: None,
                mouse_action: None,
            });
        }
        let mouse_action = find_wheel_target_pane(frame_layout, hit_region)
            .or_else(|| find_focused_pane(frame_layout))
            .and_then(|pane_id| {
                self.handle_wheel_on_pane(mouse_input, scroll_direction, mouse_frame, pane_id)
            });
        Some(WheelDecision {
            hovered_pane_id: find_content_pane(hit_region),
            mouse_action,
        })
    }

    /// Answer a wheel tick aimed at `pane_id` by the precedence
    /// [`handle_mouse_wheel`](Self::handle_mouse_wheel) documents.
    fn handle_wheel_on_pane(
        &self,
        mouse_input: MouseInput,
        scroll_direction: ScrollDirection,
        mouse_frame: &MouseFrame,
        pane_id: PaneId,
    ) -> Option<MouseAction> {
        let scroll_line_count = usize::from(self.client_config.mouse.scroll_line_count);
        let mouse_pane = find_mouse_pane(mouse_frame, pane_id)?;
        if mouse_pane.has_selection {
            return build_scroll_action(pane_id, scroll_direction, scroll_line_count);
        }
        if is_mouse_kind_reported(mouse_pane.mouse_tracking, mouse_input.mouse_kind) {
            let pane_local_cell = compute_clamped_pane_cell(
                mouse_frame.build_frame_layout(),
                pane_id,
                mouse_input.position,
            )?;
            return Some(MouseAction::Forward {
                pane_id,
                mouse_kind: mouse_input.mouse_kind,
                modifier_flags: mouse_input.modifier_flags,
                pane_local_cell,
            });
        }
        if mouse_pane.is_on_alternate_screen && mouse_pane.is_alternate_scroll_enabled {
            return resolve_scrolling_up_for_direction(scroll_direction).map(|is_scrolling_up| {
                MouseAction::AlternateScrollArrows {
                    pane_id,
                    is_scrolling_up,
                    arrow_count: scroll_line_count,
                }
            });
        }
        match self.client_config.mouse.wheel_scroll {
            WheelScroll::ScrollScrollback => {
                build_scroll_action(pane_id, scroll_direction, scroll_line_count)
            }
            WheelScroll::Ignore => None,
        }
    }

    /// Act on a left press over the region it landed on.
    fn handle_left_press(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        let frame_layout = mouse_frame.build_frame_layout();
        match resolve_hit_region(frame_layout, mouse_input.position) {
            HitRegion::Tab { tab_id } => {
                // The click reveals the tab it names, so any peek is over.
                self.tabline_peek = None;
                vec![MouseAction::Command(Box::new(Command::FocusTab(
                    FocusTabArgs {
                        focus_target: TabTarget::Id(tab_id),
                        client_id: Some(self.client_id),
                    },
                )))]
            }
            HitRegion::TablineScrollLeft => {
                self.step_tabline_peek(mouse_frame, ScrollDirection::Left);
                Vec::new()
            }
            HitRegion::TablineScrollRight => {
                self.step_tabline_peek(mouse_frame, ScrollDirection::Right);
                Vec::new()
            }
            HitRegion::StackHeader { pane_id } => {
                vec![build_focus_pane_action(self.client_id, pane_id)]
            }
            HitRegion::PaneContent { pane_id } => {
                self.press_pane_content(pane_id, mouse_input, mouse_frame, current_time)
            }
            HitRegion::PaneBorder {
                pane_id,
                side,
                corner_horizontal_side,
            } => {
                let floating_pane_slot = frame_layout.find_drawn_floating_pane_slot(pane_id);
                let can_border_move = |border_side: Direction| match floating_pane_slot {
                    // A pinned float keeps its left and top edges.
                    Some(floating_pane_slot) => {
                        !floating_pane_slot.is_pinned
                            || matches!(border_side, Direction::Right | Direction::Down)
                    }
                    // Only a real divider — one with a pane drawn beside it to
                    // resize against — begins a drag.
                    None => has_border_neighbor(mouse_frame, pane_id, border_side),
                };
                let can_resize_pane_border = self.client_config.mouse.can_resize_pane_border;
                let mut movable_border_sides = std::iter::once(side)
                    .chain(corner_horizontal_side)
                    .filter(|border_side| can_resize_pane_border && can_border_move(*border_side));
                if let Some(border_side) = movable_border_sides.next() {
                    self.left_button_gesture = Some(LeftButtonGesture::BorderResize(ResizeDrag {
                        pane_id,
                        border_side,
                        corner_horizontal_side: movable_border_sides.next(),
                        last_accepted_position: mouse_input.position,
                    }));
                }
                if floating_pane_slot.is_some() {
                    vec![build_focus_pane_action(self.client_id, pane_id)]
                } else {
                    Vec::new()
                }
            }
            HitRegion::PlacementHandle { pane_id } => {
                let Some(floating_pane_slot) = frame_layout.find_drawn_floating_pane_slot(pane_id)
                else {
                    return Vec::new();
                };
                if !floating_pane_slot.is_pinned && self.sent_floating_pane_move.is_none() {
                    self.begin_floating_pane_handle_drag(
                        pane_id,
                        floating_pane_slot.outer_rect.origin,
                        mouse_input.position,
                    );
                }
                vec![build_focus_pane_action(self.client_id, pane_id)]
            }
            HitRegion::PaneTitlebar { pane_id } => {
                vec![build_focus_pane_action(self.client_id, pane_id)]
            }
            HitRegion::PanePinToggle { pane_id } => {
                let Some(floating_pane_slot) = frame_layout.find_drawn_floating_pane_slot(pane_id)
                else {
                    return Vec::new();
                };
                vec![MouseAction::Command(Box::new(Command::SetPanePinned(
                    SetPanePinnedArgs {
                        pane_id: Some(pane_id),
                        is_pinned: !floating_pane_slot.is_pinned,
                    },
                )))]
            }
            HitRegion::PaneMinimizeButton { pane_id } => {
                vec![MouseAction::Command(Box::new(Command::SetPaneMinimized(
                    SetPaneMinimizedArgs {
                        pane_id: Some(pane_id),
                        is_minimized: true,
                    },
                )))]
            }
            HitRegion::PaneCloseButton { pane_id } => {
                vec![MouseAction::Command(Box::new(Command::ClosePane(
                    ClosePaneArgs {
                        pane_id: Some(pane_id),
                        should_force_close: false,
                        should_kill_process_tree: false,
                    },
                )))]
            }
            HitRegion::Tabline => {
                // A frame carrying no first visible tab index begins no
                // peek-drag.
                if let Some(first_visible_tab_index) = find_first_visible_tab_index(
                    self.build_frame_layout_with_current_chrome(mouse_frame),
                ) {
                    self.left_button_gesture = Some(LeftButtonGesture::TablinePeek(TablineDrag {
                        anchor_column: mouse_input.position.column,
                        anchor_first_visible_tab_index: first_visible_tab_index,
                    }));
                }
                Vec::new()
            }
            HitRegion::FloatingPaneTrayButton => {
                self.open_floating_pane_tray(Some(mouse_frame));
                Vec::new()
            }
            HitRegion::FloatingPaneTrayTile { .. }
            | HitRegion::FloatingPaneTrayToggleMinimizeAll
            | HitRegion::FloatingPaneTray
            | HitRegion::Statusline
            | HitRegion::None => Vec::new(),
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
    /// mouse-aware program such as a full-screen `vim` or `htop`. Holding
    /// `Shift` on the press also begins a highlight, even when the viewer's
    /// mode is off. The mode is read from
    /// [`Client::is_mouse_selection_enabled`], the viewer's own copy, and not
    /// from `mouse_frame`.
    fn press_pane_content(
        &mut self,
        pane_id: PaneId,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        if mouse_frame.client_snapshot.focused_pane_id != Some(pane_id) {
            return vec![build_focus_pane_action(self.client_id, pane_id)];
        }
        let mouse_tracking = find_mouse_pane(mouse_frame, pane_id)
            .map_or(MouseTracking::Off, |mouse_pane| mouse_pane.mouse_tracking);
        let is_shift_selection = mouse_input
            .modifier_flags
            .has_all_modifiers(BindingModifierFlags::SHIFT);
        if is_mouse_kind_reported(mouse_tracking, mouse_input.mouse_kind)
            && !self.is_mouse_selection_enabled
            && !is_shift_selection
        {
            return self.forward_mouse_input(mouse_input, mouse_frame);
        }
        let click_count = self.record_click(MouseButton::Left, current_time);
        self.begin_selection_drag(pane_id, mouse_input, click_count, mouse_frame)
    }

    /// Begin a selection drag in `pane_id`: record where it started and the
    /// shape `click_count` picked, drop any highlight the pane already had, and — for
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
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(anchor_grid_position) =
            self.find_grid_position_at_screen_point(mouse_frame, pane_id, mouse_input.position)
        else {
            return Vec::new();
        };
        let selection_kind = if mouse_input
            .modifier_flags
            .has_all_modifiers(BindingModifierFlags::ALT)
        {
            SelectionKind::Block
        } else {
            click_count.get_selection_kind()
        };
        let selection_drag = SelectionDrag {
            pane_id,
            selection_kind,
            anchor_grid_position,
            pointer_position: mouse_input.position,
            next_scroll_time: None,
            is_on_alternate_screen: find_mouse_pane(mouse_frame, pane_id)
                .is_some_and(|mouse_pane| mouse_pane.is_on_alternate_screen),
        };
        self.left_button_gesture = Some(LeftButtonGesture::Selection(selection_drag));
        let mut selection_actions = vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id }),
        )))];
        if matches!(selection_kind, SelectionKind::Word | SelectionKind::Line) {
            // Both ends are the press; the session grows them outward to the
            // whole word or line as it applies the highlight.
            selection_actions.extend(self.extend_selection(
                selection_drag,
                mouse_input.position,
                mouse_frame,
            ));
        }
        selection_actions
    }

    /// Route a left drag by the gesture the press began.
    fn handle_left_drag(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        match self.left_button_gesture {
            Some(LeftButtonGesture::Selection(selection_drag)) => self.drag_selection_to(
                selection_drag,
                mouse_input.position,
                mouse_frame,
                current_time,
            ),
            Some(LeftButtonGesture::BorderResize(resize_drag)) => {
                self.drag_resize_to(resize_drag, mouse_input.position)
            }
            Some(LeftButtonGesture::TablinePeek(tabline_drag)) => {
                self.drag_tabline_to(tabline_drag, mouse_frame, mouse_input.position.column);
                Vec::new()
            }
            Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) => {
                self.drag_floating_pane_to(floating_pane_handle_drag, mouse_input.position);
                Vec::new()
            }
            None => self.forward_mouse_input(mouse_input, mouse_frame),
        }
    }

    /// End the koshi gesture the left release `mouse_input` finds under way.
    /// With no gesture under way, the release belongs to the program under the
    /// pointer.
    ///
    /// **Releasing the selection IS the copy**: the viewer dispatches
    /// [`VisualCommand::Copy`] for the pane it was highlighting, and the session
    /// reads the highlighted text at that instant and puts it on the clipboard.
    /// A grab-handle release sends the move
    /// ([`submit_floating_pane_move`](Self::submit_floating_pane_move)). A
    /// border or tab-strip release sends nothing.
    fn release_mouse_gesture(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        match self.left_button_gesture.take() {
            // A plain click, whose press highlighted nothing, has no highlight
            // to copy; the session finds none and copies nothing.
            Some(LeftButtonGesture::Selection(selection_drag)) => {
                vec![MouseAction::Command(Box::new(Command::Visual(
                    VisualCommand::Copy(CopyArgs {
                        pane_id: selection_drag.pane_id,
                        should_trim_trailing_whitespace: self
                            .client_config
                            .copy
                            .should_trim_trailing_whitespace,
                    }),
                )))]
            }
            Some(LeftButtonGesture::BorderResize(_) | LeftButtonGesture::TablinePeek(_)) => {
                Vec::new()
            }
            Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) => self
                .submit_floating_pane_move_as_mouse_actions(
                    floating_pane_handle_drag,
                    mouse_input.position,
                    mouse_frame,
                ),
            None => self.forward_mouse_input(mouse_input, mouse_frame),
        }
    }

    /// Extend the selection drag to the pointer at `pointer_position`.
    ///
    /// A pointer inside the pane highlights from the anchor to the cell under
    /// it. A pointer past the top or bottom edge highlights to the pane's
    /// nearest row and arms the scroll timer, so holding it there keeps pulling
    /// more text in. A pointer only to the left or right of the pane clamps to
    /// the edge column without scrolling — there is no more text sideways.
    fn drag_selection_to(
        &mut self,
        selection_drag: SelectionDrag,
        pointer_position: Point,
        mouse_frame: &MouseFrame,
        current_time: Instant,
    ) -> Vec<MouseAction> {
        let next_scroll_time = self
            .compute_edge_scroll_direction(mouse_frame, selection_drag.pane_id, pointer_position)
            .map(|_| current_time + SELECTION_SCROLL_INTERVAL_DURATION);
        self.left_button_gesture = Some(LeftButtonGesture::Selection(SelectionDrag {
            pointer_position,
            next_scroll_time,
            ..selection_drag
        }));
        self.extend_selection(selection_drag, pointer_position, mouse_frame)
    }

    /// Highlight from `selection_drag`'s anchor to the pointer at `pointer_position`. Character and
    /// block drags mean exactly the cells named; a word or line drag is grown to
    /// whole words or lines by the session, which holds the text.
    fn extend_selection(
        &self,
        selection_drag: SelectionDrag,
        pointer_position: Point,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(cursor_grid_position) = self.find_grid_position_at_screen_point(
            mouse_frame,
            selection_drag.pane_id,
            pointer_position,
        ) else {
            return Vec::new();
        };
        vec![MouseAction::Command(Box::new(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id: selection_drag.pane_id,
                selection: Selection {
                    selection_kind: selection_drag.selection_kind,
                    anchor: selection_drag.anchor_grid_position,
                    cursor: cursor_grid_position,
                },
            }),
        )))]
    }

    /// Move the grabbed border, and the second border of a grabbed corner, to
    /// follow a drag whose pointer is now at `pointer_position`: one
    /// [`MouseAction::Resize`] for each border the pointer moved along. A
    /// bottom-right corner grabbed at `(40, 10)` and dragged to `(43, 12)` asks
    /// for `Right` 3 cells, then `Down` 2 cells.
    ///
    /// Asks for the whole distance from the anchor every time. The session
    /// moves the border as far as it can: a fast drag that jumps several cells
    /// fills right up to a pane's minimum size.
    /// [`note_resize_applied`](Self::note_resize_applied) moves the anchor over
    /// the cells the session says the pane's size changed.
    fn drag_resize_to(&self, resize_drag: ResizeDrag, pointer_position: Point) -> Vec<MouseAction> {
        std::iter::once(resize_drag.border_side)
            .chain(resize_drag.corner_horizontal_side)
            .filter_map(|border_side| {
                let resize_cell_delta = compute_resize_cell_delta(
                    border_side,
                    resize_drag.last_accepted_position,
                    pointer_position,
                );
                (resize_cell_delta != 0).then_some(MouseAction::Resize {
                    pane_id: resize_drag.pane_id,
                    border_side,
                    resize_step: resize_cell_delta.signum(),
                    requested_cell_count: resize_cell_delta.unsigned_abs(),
                })
            })
            .collect()
    }

    /// Advance the border drag's anchor over the `applied_cell_count` cells the session
    /// moved `pane_id`'s `border_side` border by, for a move asked for in direction
    /// `resize_step`. The anchor stops where the resize stopped, and a reverse drag
    /// resizes the pane the instant the pointer crosses back.
    ///
    /// Nothing moves unless the drag now held is the one `pane_id` names and
    /// grabbed `border_side`: an answer for a border the viewer has let go of,
    /// or for another border of the same pane, leaves the anchor where it is.
    /// An answer for a left or right border moves the anchor's column, and one
    /// for an up or down border moves its row.
    ///
    /// `resize_step` and `applied_cell_count` are the whole of the distance —
    /// the pointer is never read here, so an answer that lands while the
    /// pointer is still moves the anchor exactly as far as one that lands
    /// mid-motion.
    pub fn note_resize_applied(
        &mut self,
        pane_id: PaneId,
        border_side: Direction,
        resize_step: i16,
        applied_cell_count: u16,
    ) {
        let Some(LeftButtonGesture::BorderResize(resize_drag)) = self.left_button_gesture else {
            return;
        };
        let is_grabbed_border_side = resize_drag.border_side == border_side
            || resize_drag.corner_horizontal_side == Some(border_side);
        if resize_drag.pane_id != pane_id || !is_grabbed_border_side || applied_cell_count == 0 {
            return;
        }
        self.left_button_gesture = Some(LeftButtonGesture::BorderResize(ResizeDrag {
            last_accepted_position: advance_resize_anchor(
                border_side,
                resize_drag.last_accepted_position,
                resize_step,
                applied_cell_count,
            ),
            ..resize_drag
        }));
    }

    /// Scroll the tab strip to follow an in-flight drag whose pointer is now at
    /// column `pointer_column`. Dragging right moves the strip right (revealing earlier
    /// tabs); one tab per [`TABLINE_DRAG_CELL_COUNT`] cells.
    fn drag_tabline_to(
        &mut self,
        tabline_drag: TablineDrag,
        mouse_frame: &MouseFrame,
        pointer_column: u16,
    ) {
        let tabline_scroll_cell_delta =
            i32::from(tabline_drag.anchor_column) - i32::from(pointer_column);
        let tabline_scroll_step_count = tabline_scroll_cell_delta / TABLINE_DRAG_CELL_COUNT;
        let target_first_visible_tab_index = (tabline_drag.anchor_first_visible_tab_index as i32
            + tabline_scroll_step_count)
            .max(0) as usize;
        self.peek_tabline(mouse_frame, target_first_visible_tab_index);
    }

    /// Peek this viewer's tab strip one tab on from the first tab it shows with
    /// this viewer's current chrome, which holds every peek made since the last
    /// paint. Up and left step toward the first tab, down and right toward the
    /// last. A frame that draws no tab strip leaves the peek as it is.
    ///
    /// Two right-arrow clicks before a repaint, on a strip that showed tab `0`
    /// first, peek from tab `2`.
    fn step_tabline_peek(&mut self, mouse_frame: &MouseFrame, scroll_direction: ScrollDirection) {
        let Some(first_visible_tab_index) =
            find_first_visible_tab_index(self.build_frame_layout_with_current_chrome(mouse_frame))
        else {
            return;
        };
        let target_first_visible_tab_index = match scroll_direction {
            ScrollDirection::Up | ScrollDirection::Left => {
                first_visible_tab_index.saturating_sub(1)
            }
            ScrollDirection::Down | ScrollDirection::Right => first_visible_tab_index + 1,
        };
        self.peek_tabline(mouse_frame, target_first_visible_tab_index);
    }

    /// Peek this viewer's tab strip from tab index `target_first_visible_tab_index`, recorded
    /// against the tab the frame is showing. A tab switch cancels the peek. The renderer clamps
    /// an index past the last tab.
    fn peek_tabline(&mut self, mouse_frame: &MouseFrame, target_first_visible_tab_index: usize) {
        self.tabline_peek = Some(TablinePeek {
            active_tab_id: mouse_frame.client_snapshot.active_tab_id,
            first_visible_tab_index: target_first_visible_tab_index,
        });
    }

    /// Record a press into this viewer's run of clicks and report what it makes:
    /// one click, two, or three.
    ///
    /// **The gap between presses decides this.** Pressing a different button
    /// always starts a new run: a left click followed by a quick right click is
    /// not a double click.
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
    /// A button gesture is captured: a forwarded press records its pane, its
    /// button and its cell, and the drags and release that follow go to that
    /// same pane even as the pointer leaves it. A forwarded press first ends
    /// the capture already held, whose program is owed a release
    /// ([`end_mouse_capture`](Self::end_mouse_capture)). Each forwarded drag
    /// records its cell. Any release ends the capture and goes to the captured
    /// pane as a release of the captured button, whichever button it names.
    ///
    /// A press or a bare move goes to the focused pane under the pointer. A
    /// drag or release with no capture — the press was koshi's, it focused
    /// nothing, or the pane's tracking level did not ask for it — is dropped.
    /// No program receives a release without its press.
    ///
    /// A press or a bare move outside the pane's content reaches no program: it
    /// names no cell there. A captured drag or release does reach it, at the
    /// content cell nearest the pointer, so a gesture that wandered off the
    /// pane still ends inside it. A release whose pane has no content cell in
    /// `mouse_frame` goes out at the capture's last cell.
    ///
    /// The pane's tracking level from the painted frame gates a press, a drag
    /// and a bare move. A captured release is forwarded whatever that level
    /// says. The session reads the live level before it writes, and a release
    /// ends the button it holds even when that level writes no release.
    ///
    /// Example: the frame shows `vim` in X10 mode (`?9`), which reports
    /// presses only. A left press is forwarded and captured. The left release
    /// is forwarded too, and the session writes no bytes for it.
    fn forward_mouse_input(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let frame_layout = mouse_frame.build_frame_layout();
        if let MouseKind::Release(_) = mouse_input.mouse_kind {
            let Some(mouse_capture) = self.mouse_capture.take() else {
                return Vec::new();
            };
            let pane_local_cell = compute_clamped_pane_cell(
                frame_layout,
                mouse_capture.pane_id,
                mouse_input.position,
            )
            .unwrap_or(mouse_capture.last_pane_local_cell);
            return vec![MouseAction::Forward {
                pane_id: mouse_capture.pane_id,
                mouse_kind: MouseKind::Release(mouse_capture.button),
                modifier_flags: mouse_input.modifier_flags,
                pane_local_cell,
            }];
        }
        let (pane_id, mouse_kind) = match mouse_input.mouse_kind {
            MouseKind::Press(_) | MouseKind::Motion => match find_focused_pane(frame_layout) {
                Some(focused_pane_id)
                    if resolve_hit_region(frame_layout, mouse_input.position)
                        == (HitRegion::PaneContent {
                            pane_id: focused_pane_id,
                        }) =>
                {
                    (focused_pane_id, mouse_input.mouse_kind)
                }
                _ => return Vec::new(),
            },
            // A captured drag is re-stamped with the button its press named.
            MouseKind::Drag(_) => match self.mouse_capture {
                Some(mouse_capture) => {
                    (mouse_capture.pane_id, MouseKind::Drag(mouse_capture.button))
                }
                None => return Vec::new(),
            },
            MouseKind::Release(_) | MouseKind::Scroll(_) => return Vec::new(),
        };
        let Some(mouse_pane) = find_mouse_pane(mouse_frame, pane_id) else {
            return Vec::new();
        };
        if !is_mouse_kind_reported(mouse_pane.mouse_tracking, mouse_kind) {
            return Vec::new();
        }
        let Some(pane_local_cell) =
            compute_clamped_pane_cell(frame_layout, pane_id, mouse_input.position)
        else {
            return Vec::new();
        };
        match mouse_kind {
            MouseKind::Press(pressed_button) => {
                self.end_mouse_capture();
                self.mouse_capture = Some(MouseCapture {
                    pane_id,
                    button: pressed_button,
                    last_pane_local_cell: pane_local_cell,
                });
            }
            MouseKind::Drag(_) => {
                if let Some(mouse_capture) = self.mouse_capture.as_mut() {
                    mouse_capture.last_pane_local_cell = pane_local_cell;
                }
            }
            MouseKind::Release(_) | MouseKind::Motion | MouseKind::Scroll(_) => {}
        }
        vec![MouseAction::Forward {
            pane_id,
            mouse_kind,
            modifier_flags: mouse_input.modifier_flags,
            pane_local_cell,
        }]
    }

    /// Drop any gesture `mouse_frame`, the frame this viewer just painted,
    /// ended. Called once for each painted frame, before any mouse event is
    /// placed against it.
    ///
    /// A gesture aimed at a pane the frame no longer draws ends where it
    /// stands: a pane that closed, was hidden, or left with a tab switch. A
    /// capture ended this way leaves its program owed a release
    /// (`end_mouse_capture`).
    ///
    /// A selection drag also ends when its pane swapped between the primary and
    /// the alternate screen since the press. Its anchor names a line of the
    /// screen the press landed on.
    ///
    /// Floating pane move mode drops the cell its arrow keys moved the
    /// selected pane to when the frame does not draw that pane or draws it
    /// pinned. The mode and its selected pane stay, and nothing reaches the
    /// session.
    ///
    /// Example: a left press in `vim` in a floating pane holds a capture with
    /// last cell `(4, 2)`. A key hides the floating panes, and the next frame
    /// does not draw that pane. That frame ends the capture, and
    /// `take_mouse_capture_release` gives a left release at `(4, 2)`.
    pub fn drop_gestures_the_frame_ended(&mut self, mouse_frame: &MouseFrame) {
        self.left_button_gesture =
            self.left_button_gesture
                .filter(|left_button_gesture| match left_button_gesture {
                    LeftButtonGesture::Selection(selection_drag) => {
                        is_pane_drawn(mouse_frame, selection_drag.pane_id)
                            && find_mouse_pane(mouse_frame, selection_drag.pane_id).is_none_or(
                                |mouse_pane| {
                                    mouse_pane.is_on_alternate_screen
                                        == selection_drag.is_on_alternate_screen
                                },
                            )
                    }
                    LeftButtonGesture::BorderResize(resize_drag) => {
                        is_pane_drawn(mouse_frame, resize_drag.pane_id)
                    }
                    LeftButtonGesture::TablinePeek(_) => true,
                    LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag) => {
                        is_pane_drawn(mouse_frame, floating_pane_handle_drag.pane_id)
                    }
                });
        if self
            .mouse_capture
            .is_some_and(|mouse_capture| !is_pane_drawn(mouse_frame, mouse_capture.pane_id))
        {
            self.end_mouse_capture();
        }
        if let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode.as_mut()
        {
            let is_selected_pane_movable = mouse_frame
                .build_frame_layout()
                .find_drawn_floating_pane_slot(floating_pane_move_selection.selected_pane_id)
                .is_some_and(|floating_pane_slot| !floating_pane_slot.is_pinned);
            if !is_selected_pane_movable {
                floating_pane_move_selection.draft_top_left_cell = None;
            }
        }
    }

    /// Update the pointer hover ([`update_pointer_hover`](Self::update_pointer_hover))
    /// and the placement tab hover from `hit_region`, the region under
    /// `mouse_input`'s position: a tab region starts the hover of that tab,
    /// and any other region clears it.
    fn update_placement_hovers(
        &mut self,
        mouse_frame: &MouseFrame,
        mouse_input: MouseInput,
        hit_region: HitRegion,
        current_time: Instant,
    ) {
        self.update_pointer_hover(mouse_frame, mouse_input, hit_region);
        let hovered_tab_id = match hit_region {
            HitRegion::Tab { tab_id } => Some(tab_id),
            _ => None,
        };
        self.update_placement_tab_hover(hovered_tab_id, current_time);
    }

    /// Update the viewer-owned hover state from `hit_region`, the region under
    /// `mouse_input`'s position.
    ///
    /// The hovered pane is the pane whose content, top border or placement
    /// handle the pointer is on. The placement handle hover is
    /// [`FloatingHandle`](PlacementHandleHover::FloatingHandle) on a drawn
    /// floating pane's grab handle, with the pin state that frame draws. On a
    /// tiled pane's top border between its
    /// corners, a pointer motion takes [`find_tiled_placement_handle_hover`]'s
    /// answer, and every other event leaves no placement handle hover. Every
    /// other region leaves none.
    fn update_pointer_hover(
        &mut self,
        mouse_frame: &MouseFrame,
        mouse_input: MouseInput,
        hit_region: HitRegion,
    ) {
        let hovered_pane_id = match hit_region {
            HitRegion::PaneContent { pane_id }
            | HitRegion::PlacementHandle { pane_id }
            | HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
                ..
            } => Some(pane_id),
            _ => find_content_pane(hit_region),
        };
        self.hovered_pane_id = hovered_pane_id;
        let is_pointer_motion = mouse_input.mouse_kind == MouseKind::Motion;
        let drawn_floating_pane_slot = match hit_region {
            HitRegion::PlacementHandle { pane_id } => mouse_frame
                .build_frame_layout()
                .find_drawn_floating_pane_slot(pane_id),
            _ => None,
        };
        self.placement_handle_hover = match (hit_region, drawn_floating_pane_slot) {
            (_, Some(floating_pane_slot)) => Some(PlacementHandleHover::FloatingHandle {
                pane_id: floating_pane_slot.pane_id,
                is_pinned: floating_pane_slot.is_pinned,
            }),
            (
                HitRegion::PlacementHandle { pane_id }
                | HitRegion::PaneBorder {
                    pane_id,
                    side: Direction::Up,
                    ..
                },
                None,
            ) if is_pointer_motion => {
                find_tiled_placement_handle_hover(mouse_frame, pane_id, mouse_input.position)
            }
            _ => None,
        };
    }

    /// Start a grab-handle drag of floating pane `pane_id`, drawn with its
    /// top-left cell at `drawn_top_left_cell`, from the left press at
    /// `press_position`.
    fn begin_floating_pane_handle_drag(
        &mut self,
        pane_id: PaneId,
        drawn_top_left_cell: Point,
        press_position: Point,
    ) {
        self.left_button_gesture = Some(LeftButtonGesture::FloatingPaneMove(
            FloatingPaneHandleDrag {
                pane_id,
                top_left_cell: drawn_top_left_cell,
                press_position,
                press_top_left_cell: drawn_top_left_cell,
            },
        ));
    }

    /// Move the held grab handle's draft by the cells the pointer, now at
    /// `pointer_position`, moved since the press.
    ///
    /// The draft's cell may lie past the pane area: the frame draws the pane
    /// moved inside it ([`FrameLayout::list_drawn_floating_pane_slots`]). A
    /// pane pressed at `(10, 4)`, with the pointer moved from `(12, 5)` to
    /// `(9, 2)`, gets the draft `(7, 1)`. Nothing reaches the session.
    fn drag_floating_pane_to(
        &mut self,
        floating_pane_handle_drag: FloatingPaneHandleDrag,
        pointer_position: Point,
    ) {
        let FloatingPaneHandleDrag {
            press_position,
            press_top_left_cell,
            ..
        } = floating_pane_handle_drag;
        let column_delta = i32::from(pointer_position.column) - i32::from(press_position.column);
        let row_delta = i32::from(pointer_position.row) - i32::from(press_position.row);
        self.left_button_gesture = Some(LeftButtonGesture::FloatingPaneMove(
            FloatingPaneHandleDrag {
                top_left_cell: Point {
                    column: shift_cell_coordinate(press_top_left_cell.column, column_delta),
                    row: shift_cell_coordinate(press_top_left_cell.row, row_delta),
                },
                ..floating_pane_handle_drag
            },
        ));
    }

    /// End the grab-handle drag `floating_pane_handle_drag` on a left release
    /// at `release_position`, and return what the release sends.
    ///
    /// The release first moves the draft to `release_position`, as a drag does
    /// ([`drag_floating_pane_to`](Self::drag_floating_pane_to)). The move then
    /// names the top-left cell the frame draws the pane at, inside the pane
    /// area. A release at the cell the press landed on, a pane drawn back at
    /// the cell the press found it at, and a pane the frame no longer draws or
    /// this viewer pinned, send nothing, and the pane is drawn where the frame
    /// places it. Any other release sends one `MoveFloatingPane` under a new
    /// command id, returned with its arguments. The viewer keeps drawing the
    /// pane at the sent cell until the session rejects the move, or until it
    /// applies a frame that names the move
    /// ([`settle_floating_pane_move`](Self::settle_floating_pane_move)).
    ///
    /// A `40`-column pane drawn at column `30` of an `80`-column pane area,
    /// pressed at `(32, 6)`, sends nothing on a release at `(32, 6)`, even
    /// after the pane area shrinks to `60` columns and the frame draws the pane
    /// at column `20`.
    fn submit_floating_pane_move(
        &mut self,
        floating_pane_handle_drag: FloatingPaneHandleDrag,
        release_position: Point,
        mouse_frame: &MouseFrame,
    ) -> Option<(CommandId, MoveFloatingPaneArgs)> {
        let FloatingPaneHandleDrag {
            pane_id,
            press_position,
            press_top_left_cell,
            ..
        } = floating_pane_handle_drag;
        self.drag_floating_pane_to(floating_pane_handle_drag, release_position);
        let drawn_floating_pane_slot = self
            .build_frame_layout_with_current_chrome(mouse_frame)
            .find_drawn_floating_pane_slot(pane_id);
        self.left_button_gesture = None;
        let floating_pane_slot = drawn_floating_pane_slot?;
        let top_left_cell = floating_pane_slot.outer_rect.origin;
        if release_position == press_position
            || floating_pane_slot.is_pinned
            || top_left_cell == press_top_left_cell
        {
            return None;
        }
        let command_id = self.record_sent_floating_pane_move(pane_id, top_left_cell);
        Some((
            command_id,
            MoveFloatingPaneArgs {
                pane_id,
                top_left_cell,
            },
        ))
    }

    /// [`submit_floating_pane_move`](Self::submit_floating_pane_move), as the
    /// mouse actions this viewer writes: one [`MouseAction::MoveFloatingPane`],
    /// or none.
    fn submit_floating_pane_move_as_mouse_actions(
        &mut self,
        floating_pane_handle_drag: FloatingPaneHandleDrag,
        release_position: Point,
        mouse_frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        self.submit_floating_pane_move(floating_pane_handle_drag, release_position, mouse_frame)
            .map(|(command_id, command_args)| MouseAction::MoveFloatingPane {
                command_id,
                command_args,
            })
            .into_iter()
            .collect()
    }

    /// Record a floating pane move of `pane_id` to `top_left_cell` under a new
    /// command id as the move this viewer sent and draws ahead of the
    /// session, and return that command id. While floating pane move mode is
    /// on, the mode drops its draft and waits for this move
    /// ([`end_floating_pane_move_wait`](Self::end_floating_pane_move_wait)).
    pub(crate) fn record_sent_floating_pane_move(
        &mut self,
        pane_id: PaneId,
        top_left_cell: Point,
    ) -> CommandId {
        let command_id = CommandId::new();
        self.sent_floating_pane_move = Some(SentFloatingPaneMove {
            pane_id,
            top_left_cell,
            command_id,
        });
        if let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode.as_mut()
        {
            floating_pane_move_selection.draft_top_left_cell = None;
            floating_pane_move_selection.pending_move_command_id = Some(command_id);
        }
        command_id
    }

    /// Whether the held left button drives a floating pane's grab-handle drag.
    pub(crate) fn is_floating_pane_move_drag_active(&self) -> bool {
        matches!(
            self.left_button_gesture,
            Some(LeftButtonGesture::FloatingPaneMove(_))
        )
    }

    /// Whether floating pane move mode is on.
    fn is_floating_pane_move_mode_on(&self) -> bool {
        matches!(self.pane_move_mode, Some(PaneMoveMode::FloatingPaneMove(_)))
    }

    /// Drop the floating pane handle drag under way and return `true`, or
    /// return `false` when no handle drag is under way. A move already sent
    /// stays. The pane is drawn where the frame places it again, and nothing
    /// reaches the session. In dock/undock mode, the pane placement drag and
    /// the dock target end with it.
    pub(crate) fn cancel_floating_pane_move_drag(&mut self) -> bool {
        let is_handle_drag_cancelled = self
            .left_button_gesture
            .take_if(|left_button_gesture| {
                matches!(left_button_gesture, LeftButtonGesture::FloatingPaneMove(_))
            })
            .is_some();
        if is_handle_drag_cancelled && self.dock_undock_mode.is_some() {
            self.end_placement_drag();
            self.clear_mouse_placement_target();
        }
        is_handle_drag_cancelled
    }

    /// Move the grab-handle drag of floating pane `pane_id`: the pane starts
    /// at `drawn_top_left_cell` and moves by the pointer's travel since the
    /// press. Returns the screen cell of that press. Returns `None`,
    /// changing nothing, when no grab-handle drag of `pane_id` is under way.
    ///
    /// A drag that moved a pane from `(4, 2)` to `(7, 4)` and is rebased to
    /// `(30, 2)` draws the pane at `(33, 4)`.
    pub(crate) fn rebase_floating_pane_handle_drag(
        &mut self,
        pane_id: PaneId,
        drawn_top_left_cell: Point,
    ) -> Option<Point> {
        let Some(LeftButtonGesture::FloatingPaneMove(floating_pane_handle_drag)) =
            self.left_button_gesture.as_mut()
        else {
            return None;
        };
        if floating_pane_handle_drag.pane_id != pane_id {
            return None;
        }
        let column_delta = i32::from(floating_pane_handle_drag.top_left_cell.column)
            - i32::from(floating_pane_handle_drag.press_top_left_cell.column);
        let row_delta = i32::from(floating_pane_handle_drag.top_left_cell.row)
            - i32::from(floating_pane_handle_drag.press_top_left_cell.row);
        floating_pane_handle_drag.press_top_left_cell = drawn_top_left_cell;
        floating_pane_handle_drag.top_left_cell = Point {
            column: shift_cell_coordinate(drawn_top_left_cell.column, column_delta),
            row: shift_cell_coordinate(drawn_top_left_cell.row, row_delta),
        };
        Some(floating_pane_handle_drag.press_position)
    }

    /// Stop drawing the sent floating pane move ahead of the session when
    /// `last_applied_move_command_id`, read off the frame being applied, names
    /// it: that frame places the pane from the move on. A sent move the frame
    /// does not name, and a handle drag under way, are left as they are.
    ///
    /// When this frame names the sent move, or a resync or a new link dropped
    /// the sent move before this frame, floating pane move mode's wait for
    /// the move it sent ends
    /// ([`end_floating_pane_move_wait`](Self::end_floating_pane_move_wait)).
    /// The move ran when `last_applied_move_command_id` names it, and counts
    /// as rejected otherwise.
    pub(crate) fn settle_floating_pane_move(
        &mut self,
        last_applied_move_command_id: Option<CommandId>,
    ) {
        self.sent_floating_pane_move
            .take_if(|sent_floating_pane_move| {
                Some(sent_floating_pane_move.command_id) == last_applied_move_command_id
            });
        if self.sent_floating_pane_move.is_some() {
            return;
        }
        let Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            pending_move_command_id: Some(pending_move_command_id),
            ..
        })) = self.pane_move_mode
        else {
            return;
        };
        self.end_floating_pane_move_wait(
            Some(pending_move_command_id) == last_applied_move_command_id,
        );
    }

    /// Drop the sent floating pane move `command_id` names, which the session
    /// rejected, end floating pane move mode's wait for it as a rejected move
    /// ([`end_floating_pane_move_wait`](Self::end_floating_pane_move_wait)),
    /// and return `true`. Returns `false`, changing nothing, when `command_id`
    /// names no sent move of this viewer.
    pub(crate) fn reject_floating_pane_move(&mut self, command_id: CommandId) -> bool {
        let is_sent_move_rejected = self
            .sent_floating_pane_move
            .take_if(|sent_floating_pane_move| sent_floating_pane_move.command_id == command_id)
            .is_some();
        if is_sent_move_rejected {
            self.end_floating_pane_move_wait(false);
        }
        is_sent_move_rejected
    }

    /// End floating pane move mode's wait for the move it sent. With
    /// `stay-in-pane-placement-mode-after-placement #false`, a move the
    /// session ran (`is_move_accepted` `true`) ends the mode. A rejected move
    /// (`false`), and the setting `#true`, leave the mode on with its selected
    /// pane. Does nothing when the mode waits for no move.
    fn end_floating_pane_move_wait(&mut self, is_move_accepted: bool) {
        let Some(PaneMoveMode::FloatingPaneMove(floating_pane_move_selection)) =
            self.pane_move_mode.as_mut()
        else {
            return;
        };
        if floating_pane_move_selection
            .pending_move_command_id
            .take()
            .is_none()
        {
            return;
        }
        if is_move_accepted
            && !self
                .client_config
                .should_stay_in_pane_placement_mode_after_placement
        {
            self.close_pane_move_mode();
        }
    }

    /// Drop the sent floating pane move after the session dropped events, which
    /// may hold its rejection: the next frame shows the pane where the session
    /// places it. A handle drag under way is left as it is.
    pub(crate) fn forget_sent_floating_pane_move(&mut self) {
        self.sent_floating_pane_move = None;
    }

    /// The position in `pane_id`'s text that the screen cell `screen_point` names, with a
    /// point outside the pane pulled to its nearest edge so a drag that left the
    /// pane still selects up to it.
    ///
    /// The row is absolute: the frame says which line the pane's top visible row
    /// is, and the `n`-th visible row is that line plus `n`. Output arriving
    /// between the paint and the press does not shift the line the press names.
    fn find_grid_position_at_screen_point(
        &self,
        mouse_frame: &MouseFrame,
        pane_id: PaneId,
        screen_point: Point,
    ) -> Option<GridPosition> {
        let pane_local_cell =
            compute_clamped_pane_cell(mouse_frame.build_frame_layout(), pane_id, screen_point)?;
        let view_top_row_index = find_mouse_pane(mouse_frame, pane_id)?.view_top_row_index;
        Some(GridPosition {
            row_index: view_top_row_index + u64::from(pane_local_cell.row),
            column_index: pane_local_cell.column,
        })
    }

    /// Which way the view must scroll for a drag held at `screen_point`: `-1` above the
    /// pane's first row, `1` below its last, and `None` while the pointer is
    /// level with the pane.
    ///
    /// Only the vertical edges scroll. Past the left or right edge the highlight
    /// clamps to the edge column and stays put.
    fn compute_edge_scroll_direction(
        &self,
        mouse_frame: &MouseFrame,
        pane_id: PaneId,
        screen_point: Point,
    ) -> Option<i8> {
        let content_rect = find_pane_content_rect(mouse_frame.build_frame_layout(), pane_id)?;
        let bottom_row_index = content_rect
            .origin
            .row
            .saturating_add(content_rect.size.row_count.saturating_sub(1));
        if screen_point.row < content_rect.origin.row {
            Some(-1)
        } else if screen_point.row > bottom_row_index {
            Some(1)
        } else {
            None
        }
    }

    /// `mouse_frame` borrowed with this viewer's chrome as it stands now, in
    /// place of the chrome `mouse_frame` was painted with. The two differ from
    /// any chrome change until the next paint: two wheel ticks over the tab
    /// strip before a paint read a strip peeked from tab `1`, not tab `0`.
    pub(crate) fn build_frame_layout_with_current_chrome<'a>(
        &self,
        mouse_frame: &'a MouseFrame,
    ) -> FrameLayout<'a> {
        let mut frame_layout = mouse_frame.build_frame_layout();
        frame_layout.viewer_chrome =
            self.build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id);
        frame_layout
    }
}

/// A `FocusPane` for `pane_id`, naming `client_id` so the switch moves that viewer's
/// focus and no other's.
fn build_focus_pane_action(client_id: ClientId, pane_id: PaneId) -> MouseAction {
    MouseAction::Command(Box::new(Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(pane_id),
        client_id: Some(client_id),
    })))
}

/// Whether `hit_region` is on the tab strip: the tabline row, the floating
/// pane tray button, a tab, or a scroll arrow.
fn is_tab_strip_region(hit_region: HitRegion) -> bool {
    matches!(
        hit_region,
        HitRegion::Tabline
            | HitRegion::FloatingPaneTrayButton
            | HitRegion::Tab { .. }
            | HitRegion::TablineScrollLeft
            | HitRegion::TablineScrollRight
    )
}

/// The pane whose content `hit_region` is, or `None` over every other region,
/// a pane's border, grab handle, titlebar and buttons included.
fn find_content_pane(hit_region: HitRegion) -> Option<PaneId> {
    match hit_region {
        HitRegion::PaneContent { pane_id } => Some(pane_id),
        _ => None,
    }
}

/// The pane a wheel tick over `hit_region` targets: the pane whose content it
/// is, or the floating pane `frame_layout` draws whose titlebar, border, grab
/// handle, pin toggle, minimize button or close button it is. `None` over
/// every other region, a tiled pane's border and placement handle included.
fn find_wheel_target_pane(frame_layout: FrameLayout<'_>, hit_region: HitRegion) -> Option<PaneId> {
    match hit_region {
        HitRegion::PaneContent { pane_id }
        | HitRegion::PaneTitlebar { pane_id }
        | HitRegion::PanePinToggle { pane_id }
        | HitRegion::PaneMinimizeButton { pane_id }
        | HitRegion::PaneCloseButton { pane_id } => Some(pane_id),
        HitRegion::PaneBorder { pane_id, .. } | HitRegion::PlacementHandle { pane_id } => {
            frame_layout
                .find_drawn_floating_pane_slot(pane_id)
                .map(|_| pane_id)
        }
        _ => None,
    }
}

/// Return an insertion into the smallest insertion span under `screen_point`,
/// on that span's edge nearest the pointer. Between spans of equal area, the
/// earlier span wins. Returns `None` when no span covers `screen_point`.
fn find_mouse_insertion_target(
    screen_point: Point,
    destination_layout_rect: RatatuiRect,
    destination_size: Size,
    destination_tab_id: TabId,
    placement_destinations: &PlacementDestinations,
) -> Option<TiledPlacementTarget> {
    placement_destinations
        .insertion_spans
        .iter()
        .filter_map(|insertion_span| {
            let span_rect = project_core_rect(
                insertion_span.span_rect,
                destination_size,
                destination_layout_rect,
            )?;
            span_rect
                .contains(Position::new(screen_point.column, screen_point.row))
                .then_some((insertion_span, span_rect))
        })
        .min_by_key(|(_, span_rect)| span_rect.area())
        .map(|(insertion_span, span_rect)| TiledPlacementTarget::Split {
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

/// The placement handle hover of the visible tiled pane `pane_id` with the
/// pointer at `screen_point` on its top border, between its corners:
/// [`TiledHandle`](PlacementHandleHover::TiledHandle) on the handle cells
/// [`compute_tiled_pane_top_border`] gives and
/// [`TiledTopBorder`](PlacementHandleHover::TiledTopBorder) on its other cells. `None`
/// when `pane_id` is no visible tiled pane, a floating pane included, and when
/// the pane is too narrow to draw a handle. On an `80`-column pane whose box
/// starts at `(0, 1)`, `(2, 1)` gives `TiledHandle` and `(10, 1)` gives
/// `TiledTopBorder`.
fn find_tiled_placement_handle_hover(
    mouse_frame: &MouseFrame,
    pane_id: PaneId,
    screen_point: Point,
) -> Option<PlacementHandleHover> {
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .find_visible_pane_slot(pane_id)?;
    let content_rect = find_pane_content_rect(mouse_frame.build_frame_layout(), pane_id)?;
    let pane_outer_rect = Rect::from_origin_and_size(
        Point {
            column: content_rect.origin.column.saturating_sub(1),
            row: content_rect.origin.row.saturating_sub(1),
        },
        Size {
            column_count: content_rect.size.column_count.saturating_add(2),
            row_count: content_rect.size.row_count.saturating_add(2),
        },
    );
    let placement_handle_rect =
        compute_tiled_pane_top_border(pane_outer_rect).placement_handle_rect?;
    if placement_handle_rect.is_point_inside(screen_point) {
        Some(PlacementHandleHover::TiledHandle { pane_id })
    } else {
        Some(PlacementHandleHover::TiledTopBorder { pane_id })
    }
}

/// Whether the `border_side` border of `pane_id` has another pane drawn right beside it in
/// `mouse_frame` — the only kind of border a drag can move.
///
/// A neighbor's box starts exactly [`TabSnapshot::gap_cell_count`] cells past the pane's
/// edge on that side and covers at least one of the same rows (or columns).
/// With `gap_cell_count` 0, a pane at columns 0–39 next to one at 40–79 has a neighbor on
/// its right; with `gap_cell_count` 2 the neighbor starts at column 42. The second pane's
/// right edge is the tab's outer frame and has none. A zoomed view draws one
/// pane and no dividers, and the boundary above a collapsed stack header has
/// no drawn pane on the far side; neither is draggable.
///
/// [`TabSnapshot::gap_cell_count`]: koshi_renderer::snapshot::TabSnapshot::gap_cell_count
fn has_border_neighbor(mouse_frame: &MouseFrame, pane_id: PaneId, border_side: Direction) -> bool {
    let Some(pane_outer_rect) = mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .find_visible_pane_slot(pane_id)
        .map(|pane_slot| pane_slot.outer_rect)
    else {
        return false;
    };
    let gap_cell_count = mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count;
    mouse_frame
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
                        == (pane_outer_rect.origin.column + pane_outer_rect.size.column_count)
                            .saturating_add(gap_cell_count)
                        && has_row_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Left => {
                    (neighbor_rect.origin.column + neighbor_rect.size.column_count)
                        .saturating_add(gap_cell_count)
                        == pane_outer_rect.origin.column
                        && has_row_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Down => {
                    neighbor_rect.origin.row
                        == (pane_outer_rect.origin.row + pane_outer_rect.size.row_count)
                            .saturating_add(gap_cell_count)
                        && has_column_overlap(pane_outer_rect, neighbor_rect)
                }
                Direction::Up => {
                    (neighbor_rect.origin.row + neighbor_rect.size.row_count)
                        .saturating_add(gap_cell_count)
                        == pane_outer_rect.origin.row
                        && has_column_overlap(pane_outer_rect, neighbor_rect)
                }
            }
        })
}

/// Whether `mouse_frame` draws `pane_id`: a visible slot of its active tab, or
/// a floating pane this viewer draws.
fn is_pane_drawn(mouse_frame: &MouseFrame, pane_id: PaneId) -> bool {
    mouse_frame
        .session_snapshot
        .active_tab_snapshot
        .find_visible_pane_slot(pane_id)
        .is_some()
        || mouse_frame
            .build_frame_layout()
            .find_drawn_floating_pane_slot(pane_id)
            .is_some()
}

/// Whether two pane boxes cover any of the same rows.
fn has_row_overlap(first_pane_rect: Rect, second_pane_rect: Rect) -> bool {
    first_pane_rect.origin.row < second_pane_rect.origin.row + second_pane_rect.size.row_count
        && second_pane_rect.origin.row < first_pane_rect.origin.row + first_pane_rect.size.row_count
}

/// Whether two pane boxes cover any of the same columns.
fn has_column_overlap(first_pane_rect: Rect, second_pane_rect: Rect) -> bool {
    first_pane_rect.origin.column
        < second_pane_rect.origin.column + second_pane_rect.size.column_count
        && second_pane_rect.origin.column
            < first_pane_rect.origin.column + first_pane_rect.size.column_count
}

/// The frame's entry for `pane_id`, or `None` when the frame carried no content for
/// it.
fn find_mouse_pane(mouse_frame: &MouseFrame, pane_id: PaneId) -> Option<&MousePane> {
    mouse_frame
        .mouse_panes
        .iter()
        .find(|mouse_pane| mouse_pane.pane_id == pane_id)
}

/// This client's focused pane when `frame_layout` places it — a slot of the
/// active tab, or a floating pane this viewer draws. It is the pane an event
/// over chrome falls through to. `None` when no pane is focused or the frame
/// places it nowhere.
fn find_focused_pane(frame_layout: FrameLayout<'_>) -> Option<PaneId> {
    let focused_pane_id = frame_layout.client_snapshot.focused_pane_id?;
    let is_focused_pane_placed = frame_layout
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .any(|pane_slot| pane_slot.pane_id == focused_pane_id)
        || frame_layout
            .find_drawn_floating_pane_slot(focused_pane_id)
            .is_some();
    is_focused_pane_placed.then_some(focused_pane_id)
}

/// The screen rectangle of the pane area `mouse_frame` was painted with. An
/// `80x24` viewport with the tabline on row 0 and the statusline on row 23
/// gives `x: 0, y: 1, width: 80, height: 22`.
fn compute_mouse_frame_pane_area(mouse_frame: &MouseFrame) -> RatatuiRect {
    let viewport_size = mouse_frame.committed_regions.viewport_size;
    compute_pane_area(
        &mouse_frame.committed_regions,
        RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count),
    )
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

/// Cells the pointer at `end_position` has moved from `start_position` toward
/// the grabbed `border_side`, signed for [`Command::ResizePane`]: positive
/// grows the pane (its border moves outward), negative shrinks it. Left/right
/// borders read the x axis, up/down borders read the y axis; motion on the
/// other axis is ignored.
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

/// The cell `start_position` reaches when `accepted_cell_count` cells of a border move asked for in
/// direction `resize_step` are accepted. The inverse of [`compute_resize_cell_delta`]: a positive
/// `resize_step` 1 grows the pane, which walks a left or up border toward zero and a
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

/// `cell_coordinate` moved `cell_delta` cells, held between `0` and
/// `u16::MAX`: `3` moved `-5` cells is `0`.
fn shift_cell_coordinate(cell_coordinate: u16, cell_delta: i32) -> u16 {
    (i32::from(cell_coordinate) + cell_delta).clamp(0, i32::from(u16::MAX)) as u16
}
