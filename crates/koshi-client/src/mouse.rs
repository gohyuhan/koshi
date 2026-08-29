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
//! Mouse-select mode is the one piece of session state read live rather than
//! from the frame: [`Client::mouse_select`] follows the session's report of the
//! toggle, so a press right after the key that flipped it routes the new way
//! without waiting for a paint.
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

use koshi_config::types::WheelScroll;
use koshi_core::command::{
    ClearSelectionArgs, Command, CopyArgs, CopyTarget, FocusPaneArgs, FocusTabArgs, FocusTarget,
    GridPos, Selection, SelectionKind, SetSelectionArgs, TabTarget, VisualCommand,
};
use koshi_core::geometry::{Direction, Point};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::mouse::{
    reports, MouseButton, MouseInput, MouseKind, MouseTracking, ScrollDirection,
};
use koshi_renderer::snapshot::{MouseFrame, MousePane, PaneKind, PaneSlot, ViewerChrome};
use koshi_renderer::{
    hit_test, pane_cell_clamped, pane_content_rect, tabline_first_visible, HitRegion,
};

use crate::Client;

#[cfg(test)]
mod tests;

/// How long after a press a second press still counts as a double click, and a
/// third as a triple.
///
/// A mouse reports a double click as two ordinary presses — no terminal
/// protocol carries a click count — so this gap is the only thing that tells one
/// from two deliberate clicks. Matches the 400ms alacritty settled on.
const CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// How often the view scrolls while a selection drag is held past a pane's top
/// or bottom edge. Matches alacritty's selection-scrolling interval.
const SELECTION_SCROLL_INTERVAL: Duration = Duration::from_millis(15);

/// Lines the view scrolls per firing while a drag is held past an edge.
const SELECTION_SCROLL_LINES: usize = 1;

/// Cells of horizontal drag that scroll the tab strip by one tab.
pub const TABLINE_DRAG_STEP: i32 = 6;

/// What the viewer decided one wheel tick means: where the pointer is, and what
/// the session must do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelDecision {
    /// The pane under the pointer, or `None` over koshi's own chrome. The
    /// viewer marks it hovered so the renderer can color the wheel's target.
    pub hovered: Option<PaneId>,
    /// What to do, or `None` when this tick does nothing — a horizontal wheel
    /// where only a vertical one acts, or a plain pane under a viewer whose
    /// `mouse.wheel` setting is `ignore`.
    pub action: Option<MouseAction>,
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
        pane: PaneId,
        /// Up into history, or down toward live output.
        up: bool,
        /// Lines to move, from this viewer's `mouse.scroll_lines`.
        lines: usize,
    },
    /// Hand the event to the program in `pane` as a mouse report. The session
    /// encodes it from that pane's live tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane: PaneId,
        /// The event, with the cell it landed on and the modifiers held.
        mouse: MouseInput,
    },
    /// Send `count` cursor arrow keys to `pane` — the alternate-scroll (`?1007`)
    /// translation of a wheel tick on the alternate screen.
    AltScrollArrows {
        /// The pane whose program receives the arrows.
        pane: PaneId,
        /// Up-arrows, or down-arrows.
        up: bool,
        /// How many, from this viewer's `mouse.scroll_lines`.
        count: usize,
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
        pane: PaneId,
        /// Which of the pane's borders was grabbed.
        side: Direction,
        /// `1` grows the pane, `-1` shrinks it.
        step: i16,
        /// How many single-cell steps the pointer travelled.
        count: u16,
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
    pane: PaneId,
    /// The shape the press picked — one click characters, two words, three
    /// lines, `Alt` a block. Fixed for the whole drag.
    kind: SelectionKind,
    /// The end that stays put: the line and column the press landed on.
    anchor: GridPos,
    /// The pointer's last cell, in the viewer's own screen space. The scroll
    /// timer re-reads this to keep extending while the pointer is held still
    /// outside the pane.
    at: Point,
    /// When the view must next scroll because the pointer is being held past the
    /// pane's top or bottom edge; `None` whenever the pointer is inside.
    scroll_at: Option<Instant>,
    /// Whether the pane was showing the alternate screen when the press landed.
    /// The anchor names a line of that screen's text, so a frame reporting the
    /// other screen ends the drag.
    on_alt_screen: bool,
}

/// A pane-border drag under way: the pane whose border was grabbed, which side
/// it is, and the cell the last *accepted* resize tracked to. The tracked cell
/// advances only over the cells a resize was accepted for, so pushing the
/// pointer past a pane's minimum size leaves the border pinned at that limit and
/// a reverse drag moves it the instant the pointer crosses back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeDrag {
    /// The pane whose border is being dragged.
    pane: PaneId,
    /// Which of the pane's borders was grabbed.
    side: Direction,
    /// The cell the last accepted resize tracked to; the next drag delta is
    /// measured from here.
    last: Point,
}

/// A tab-strip peek-drag under way: the column the drag anchored on and the
/// first visible tab index at that instant. Dragging horizontally from the
/// anchor scrolls the strip without changing which tab is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablineDrag {
    /// The screen column the drag anchored on.
    anchor_x: u16,
    /// The first visible tab index when the drag began.
    anchor_first_visible: usize,
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
    fn selection_kind(self) -> SelectionKind {
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
    at: Instant,
    /// The run of clicks this press made.
    count: ClickCount,
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
    pub fn chrome(&self, active_tab: TabId) -> ViewerChrome {
        ViewerChrome {
            hovered_pane: self.hovered_pane,
            tabline_offset: self
                .tabline_peek
                .filter(|&(tab, _)| tab == active_tab)
                .map(|(_, first)| first),
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
    pub fn note_active_tab(&mut self, active_tab: TabId) {
        self.tabline_peek = self.tabline_peek.filter(|&(tab, _)| tab == active_tab);
    }

    /// Decide what mouse event `mouse` means against `frame`, the last frame
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
        mouse: MouseInput,
        frame: &MouseFrame,
        now: Instant,
    ) -> Vec<MouseAction> {
        self.note_active_tab(frame.client.active_tab);
        self.drop_gestures_the_frame_ended(frame);
        match mouse.kind {
            MouseKind::Scroll(_) => match self.handle_mouse_wheel(mouse, frame) {
                Some(decision) => {
                    self.hovered_pane = decision.hovered;
                    decision.action.into_iter().collect()
                }
                None => Vec::new(),
            },
            MouseKind::Press(MouseButton::Left) => self.left_press(mouse, frame, now),
            MouseKind::Drag(MouseButton::Left) => self.left_drag(mouse, frame, now),
            MouseKind::Release(_) => self.release(mouse, frame),
            MouseKind::Motion => {
                self.hovered_pane = pane_under(hit_test(self.frame_layout(frame), mouse.at));
                self.forward(mouse, frame)
            }
            MouseKind::Press(_) | MouseKind::Drag(_) => self.forward(mouse, frame),
        }
    }

    /// How long until a selection drag held past a pane's edge must scroll the
    /// view again, or `None` when no drag is held there. The event loop blocks
    /// no longer than this, so a still pointer keeps pulling text in.
    #[must_use]
    pub fn next_mouse_wakeup(&self, now: Instant) -> Option<Duration> {
        self.selection_drag
            .and_then(|drag| drag.scroll_at)
            .map(|at| at.saturating_duration_since(now))
    }

    /// Scroll a selection drag held past a pane's edge, if its next step is due
    /// at `now`.
    ///
    /// Each firing moves the view one line toward the pointer. The highlight is
    /// re-extended in [`note_scroll_applied`](Self::note_scroll_applied), once
    /// the session has said where the view landed — the extension has to cover
    /// the line the scroll just revealed, and only the session knows whether
    /// there was one.
    ///
    /// Nothing due, or a pointer no longer outside the pane, yields no actions
    /// and disarms; the next drag event arms it again.
    pub fn expire_mouse_scroll(&mut self, now: Instant, frame: &MouseFrame) -> Vec<MouseAction> {
        let Some(drag) = self.selection_drag else {
            return Vec::new();
        };
        if drag.scroll_at.is_none_or(|at| at > now) {
            return Vec::new();
        }
        let direction = self.edge_scroll_direction(frame, drag.pane, drag.at);
        let top = pane_modes(frame, drag.pane).map(|pane| pane.view_top_row);
        let (Some(direction), Some(top)) = (direction, top) else {
            // The pane moved out from under a pointer that is no longer outside
            // it; stop scrolling and let the next drag event drive.
            self.selection_drag = Some(SelectionDrag {
                scroll_at: None,
                ..drag
            });
            return Vec::new();
        };
        self.scroll_from_top = Some(top);
        self.selection_drag = Some(SelectionDrag {
            scroll_at: Some(now + SELECTION_SCROLL_INTERVAL),
            ..drag
        });
        vec![MouseAction::Scroll {
            pane: drag.pane,
            up: direction < 0,
            lines: SELECTION_SCROLL_LINES,
        }]
    }

    /// Take in where the view landed after a scroll: `top` is the line `pane`'s
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
        pane: PaneId,
        top: Option<u64>,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(before) = self.scroll_from_top.take() else {
            return Vec::new();
        };
        let Some(drag) = self.selection_drag.filter(|drag| drag.pane == pane) else {
            return Vec::new();
        };
        let Some(after) = top.filter(|&after| after != before) else {
            self.selection_drag = Some(SelectionDrag {
                scroll_at: None,
                ..drag
            });
            return Vec::new();
        };
        let Some((col, row)) = pane_cell_clamped(self.frame_layout(frame), pane, drag.at) else {
            return Vec::new();
        };
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane,
                selection: Selection {
                    kind: drag.kind,
                    anchor: drag.anchor,
                    cursor: GridPos {
                        row: after + u64::from(row),
                        col,
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
    /// tab-strip peek-drag, and the pane a held button was captured to. The
    /// highlight a selection drag already made stands.
    ///
    /// The pane under the pointer, where the tab strip is peeked, and the line a
    /// pending edge-scroll was asked from are left as they are.
    pub fn end_mouse_gestures(&mut self) {
        self.selection_drag = None;
        self.resize_drag = None;
        self.tabline_drag = None;
        self.mouse_capture = None;
    }

    /// Decide what wheel tick `mouse` means against `frame`, the last frame this
    /// viewer painted. Returns `None` when `mouse` is not a wheel tick.
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
    /// A wheel up over a plain shell with `scroll_lines = 3` yields
    /// `Scroll { pane, up: true, lines: 3 }`; the same tick over a `vim` in
    /// normal tracking yields `Forward`.
    ///
    /// A tick over the tab strip moves this viewer's own peek and yields no
    /// action: nothing on the session changes.
    #[must_use]
    pub fn handle_mouse_wheel(
        &mut self,
        mouse: MouseInput,
        frame: &MouseFrame,
    ) -> Option<WheelDecision> {
        let MouseKind::Scroll(direction) = mouse.kind else {
            return None;
        };
        let region = hit_test(self.frame_layout(frame), mouse.at);
        if let Some(to) = self.tabline_step(frame, region, direction) {
            self.peek_tabline(frame, to);
            return Some(WheelDecision {
                hovered: None,
                action: None,
            });
        }
        let hovered = pane_under(region);
        let action = hovered
            .or_else(|| focused_terminal_pane(frame))
            .and_then(|pane| self.wheel_on_pane(mouse, direction, frame, pane));
        Some(WheelDecision { hovered, action })
    }

    /// Answer a wheel tick aimed at `pane` by the precedence
    /// [`handle_mouse_wheel`](Self::handle_mouse_wheel) documents.
    fn wheel_on_pane(
        &self,
        mouse: MouseInput,
        direction: ScrollDirection,
        frame: &MouseFrame,
        pane: PaneId,
    ) -> Option<MouseAction> {
        let lines = usize::from(self.config.mouse.scroll_lines);
        let modes = pane_modes(frame, pane)?;
        if modes.has_selection {
            return scroll(pane, direction, lines);
        }
        if reports(modes.mouse_tracking, mouse.kind) {
            return Some(MouseAction::Forward { pane, mouse });
        }
        if modes.on_alt_screen && modes.alt_scroll {
            return vertical(direction).map(|up| MouseAction::AltScrollArrows {
                pane,
                up,
                count: lines,
            });
        }
        match self.config.mouse.wheel {
            WheelScroll::ScrollScrollback => scroll(pane, direction, lines),
            WheelScroll::Ignore => None,
        }
    }

    /// Act on a left press over the region it landed on.
    fn left_press(
        &mut self,
        mouse: MouseInput,
        frame: &MouseFrame,
        now: Instant,
    ) -> Vec<MouseAction> {
        match hit_test(self.frame_layout(frame), mouse.at) {
            HitRegion::Tab { tab_id } => {
                // The click reveals the tab it names, so any peek is over.
                self.tabline_peek = None;
                vec![MouseAction::Command(Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Id(tab_id),
                    client: Some(self.id),
                }))]
            }
            HitRegion::TablineScrollLeft { to } | HitRegion::TablineScrollRight { to } => {
                self.peek_tabline(frame, to);
                Vec::new()
            }
            HitRegion::StackHeader { pane_id } => vec![focus_pane(self.id, pane_id)],
            HitRegion::PaneContent { pane_id } => {
                self.press_pane_content(pane_id, mouse, frame, now)
            }
            HitRegion::PaneBorder { pane_id, side } => {
                // Only a real divider — one with a pane drawn beside it to
                // resize against — begins a drag.
                if self.config.mouse.border_resize && border_has_neighbor(frame, pane_id, side) {
                    self.resize_drag = Some(ResizeDrag {
                        pane: pane_id,
                        side,
                        last: mouse.at,
                    });
                }
                Vec::new()
            }
            HitRegion::Tabline => {
                // `Tabline` is only hit on a frame that draws one, so the
                // window index is present.
                if let Some(first_visible) = tabline_first_visible(self.frame_layout(frame)) {
                    self.tabline_drag = Some(TablineDrag {
                        anchor_x: mouse.at.x,
                        anchor_first_visible: first_visible,
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
    /// [`Client::mouse_select`], the viewer's own copy, so a press right after
    /// the key that toggled it already routes the new way.
    fn press_pane_content(
        &mut self,
        pane_id: PaneId,
        mouse: MouseInput,
        frame: &MouseFrame,
        now: Instant,
    ) -> Vec<MouseAction> {
        if frame.client.focused_pane != Some(pane_id) {
            return vec![focus_pane(self.id, pane_id)];
        }
        let tracking =
            pane_modes(frame, pane_id).map_or(MouseTracking::Off, |pane| pane.mouse_tracking);
        let shift_select = mouse.mods.contains(ModFlags::SHIFT);
        if reports(tracking, mouse.kind) && !self.mouse_select && !shift_select {
            return self.forward(mouse, frame);
        }
        let clicks = self.record_click(MouseButton::Left, now);
        self.begin_selection_drag(pane_id, mouse, clicks, frame)
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
        mouse: MouseInput,
        clicks: ClickCount,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(anchor) = self.text_pos_at(frame, pane_id, mouse.at) else {
            return Vec::new();
        };
        let kind = if mouse.mods.contains(ModFlags::ALT) {
            SelectionKind::Block
        } else {
            clicks.selection_kind()
        };
        let drag = SelectionDrag {
            pane: pane_id,
            kind,
            anchor,
            at: mouse.at,
            scroll_at: None,
            on_alt_screen: pane_modes(frame, pane_id).is_some_and(|pane| pane.on_alt_screen),
        };
        self.selection_drag = Some(drag);
        let mut actions = vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane: pane_id }),
        ))];
        if matches!(kind, SelectionKind::Word | SelectionKind::Line) {
            // Both ends are the press; the session grows them outward to the
            // whole word or line as it applies the highlight.
            actions.extend(self.extend_selection(drag, mouse.at, frame));
        }
        actions
    }

    /// Route a left drag by the gesture the press began.
    fn left_drag(
        &mut self,
        mouse: MouseInput,
        frame: &MouseFrame,
        now: Instant,
    ) -> Vec<MouseAction> {
        if let Some(drag) = self.resize_drag {
            return self.drag_resize_to(drag, mouse.at);
        }
        if let Some(drag) = self.tabline_drag {
            self.drag_tabline_to(drag, frame, mouse.at.x);
            return Vec::new();
        }
        if let Some(drag) = self.selection_drag {
            return self.drag_selection_to(drag, mouse.at, frame, now);
        }
        self.forward(mouse, frame)
    }

    /// End whichever drag was under way. A release that ends a koshi drag is
    /// koshi's; any other release belongs to the program under the pointer.
    ///
    /// **Releasing the selection IS the copy**, as zellij ships it: the viewer
    /// dispatches [`VisualCommand::Copy`] for the pane it was highlighting, and
    /// the session reads the highlighted text at that instant — while it is
    /// exactly what the user saw — and puts it on the clipboard. A viewer whose
    /// `copy.copy_on_select` is off holds the highlight and copies nothing.
    fn release(&mut self, mouse: MouseInput, frame: &MouseFrame) -> Vec<MouseAction> {
        let selecting = self.selection_drag.take();
        let resizing = self.resize_drag.take().is_some();
        let peeking = self.tabline_drag.take().is_some();
        if !(selecting.is_some() || resizing || peeking) {
            return self.forward(mouse, frame);
        }
        // A plain click, whose press highlighted nothing, has no highlight to
        // copy; the session finds none and copies nothing.
        match selecting.filter(|_| self.config.copy.copy_on_select) {
            Some(drag) => vec![MouseAction::Command(Command::Visual(VisualCommand::Copy(
                CopyArgs {
                    pane: drag.pane,
                    target: copy_target(self.config.copy.clipboard),
                    trim_trailing_whitespace: self.config.copy.trim_trailing_whitespace,
                },
            )))],
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
        drag: SelectionDrag,
        at: Point,
        frame: &MouseFrame,
        now: Instant,
    ) -> Vec<MouseAction> {
        let scroll_at = self
            .edge_scroll_direction(frame, drag.pane, at)
            .map(|_| now + SELECTION_SCROLL_INTERVAL);
        self.selection_drag = Some(SelectionDrag {
            at,
            scroll_at,
            ..drag
        });
        self.extend_selection(drag, at, frame)
    }

    /// Highlight from `drag`'s anchor to the pointer at `at`. Character and
    /// block drags mean exactly the cells named; a word or line drag is grown to
    /// whole words or lines by the session, which holds the text.
    fn extend_selection(
        &self,
        drag: SelectionDrag,
        at: Point,
        frame: &MouseFrame,
    ) -> Vec<MouseAction> {
        let Some(cursor) = self.text_pos_at(frame, drag.pane, at) else {
            return Vec::new();
        };
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane: drag.pane,
                selection: Selection {
                    kind: drag.kind,
                    anchor: drag.anchor,
                    cursor,
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
    fn drag_resize_to(&self, drag: ResizeDrag, at: Point) -> Vec<MouseAction> {
        let total = resize_delta(drag.side, drag.last, at);
        if total == 0 {
            return Vec::new();
        }
        vec![MouseAction::Resize {
            pane: drag.pane,
            side: drag.side,
            step: total.signum(),
            count: total.unsigned_abs(),
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
    pub fn note_resize_applied(&mut self, pane: PaneId, side: Direction, step: i16, applied: u16) {
        let Some(drag) = self
            .resize_drag
            .filter(|drag| drag.pane == pane && drag.side == side)
        else {
            return;
        };
        if applied > 0 {
            self.resize_drag = Some(ResizeDrag {
                last: advance_along(drag.side, drag.last, step, applied),
                ..drag
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
    pub fn note_press_forwarded(&mut self, pane: PaneId, button: MouseButton) {
        self.mouse_capture = Some((pane, button));
    }

    /// Scroll the tab strip to follow an in-flight drag whose pointer is now at
    /// column `x`. Dragging right moves the strip right (revealing earlier
    /// tabs); one tab per [`TABLINE_DRAG_STEP`] cells.
    fn drag_tabline_to(&mut self, drag: TablineDrag, frame: &MouseFrame, x: u16) {
        let delta = i32::from(drag.anchor_x) - i32::from(x);
        let steps = delta / TABLINE_DRAG_STEP;
        let target = (drag.anchor_first_visible as i32 + steps).max(0) as usize;
        self.peek_tabline(frame, target);
    }

    /// The tab index a wheel tick over the tab strip scrolls to, or `None` when
    /// the tick did not land on the strip. Up and left step toward the first
    /// tab, down and right toward the last.
    fn tabline_step(
        &self,
        frame: &MouseFrame,
        region: HitRegion,
        direction: ScrollDirection,
    ) -> Option<usize> {
        if !matches!(
            region,
            HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. }
        ) {
            return None;
        }
        let first = tabline_first_visible(self.frame_layout(frame))?;
        Some(match direction {
            ScrollDirection::Up | ScrollDirection::Left => first.saturating_sub(1),
            ScrollDirection::Down | ScrollDirection::Right => first + 1,
        })
    }

    /// Peek this viewer's tab strip from tab index `to`, recorded against the
    /// tab the frame is showing so a later tab switch cancels it. The renderer
    /// clamps an index past the last tab, so an over-far target is harmless.
    fn peek_tabline(&mut self, frame: &MouseFrame, to: usize) {
        self.tabline_peek = Some((frame.client.active_tab, to));
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
    fn record_click(&mut self, button: MouseButton, now: Instant) -> ClickCount {
        let count = match self.last_press {
            Some(last) if last.button != button => ClickCount::Single,
            Some(last) if now.duration_since(last.at) >= CLICK_THRESHOLD => ClickCount::Single,
            Some(last) => match last.count {
                ClickCount::Single => ClickCount::Double,
                ClickCount::Double => ClickCount::Triple,
                ClickCount::Triple => ClickCount::Single,
            },
            None => ClickCount::Single,
        };
        self.last_press = Some(LastPress {
            button,
            at: now,
            count,
        });
        count
    }

    /// Hand `mouse` to the program in the pane it belongs to.
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
    /// The pane's tracking level from the painted frame gates the forward, and
    /// the session re-reads the live level before it writes.
    fn forward(&mut self, mouse: MouseInput, frame: &MouseFrame) -> Vec<MouseAction> {
        let captured = self.mouse_capture;
        // A release ends the capture, whether or not it forwards. Which button
        // released cannot be trusted (some terminals report every release as the
        // left button), so any release clears.
        if matches!(mouse.kind, MouseKind::Release(_)) {
            self.mouse_capture = None;
        }
        let (pane, kind) = match mouse.kind {
            MouseKind::Press(_) | MouseKind::Motion => match focused_terminal_pane(frame) {
                Some(pane)
                    if pane_content_rect(self.frame_layout(frame), pane)
                        .is_some_and(|rect| rect.contains(mouse.at)) =>
                {
                    (pane, mouse.kind)
                }
                _ => return Vec::new(),
            },
            // A captured drag or release is re-stamped with the button its press
            // named — the event's own button is unreliable, so the program sees
            // the same button it saw go down.
            MouseKind::Drag(_) | MouseKind::Release(_) => match captured {
                Some((pane, button)) => (pane, with_button(mouse.kind, button)),
                None => return Vec::new(),
            },
            MouseKind::Scroll(_) => return Vec::new(),
        };
        let Some(modes) = pane_modes(frame, pane) else {
            return Vec::new();
        };
        if !reports(modes.mouse_tracking, kind) {
            return Vec::new();
        }
        vec![MouseAction::Forward {
            pane,
            mouse: MouseInput { kind, ..mouse },
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
        let drawn = |pane: PaneId| drawn_slot(frame, pane).is_some();
        self.selection_drag = self.selection_drag.filter(|drag| {
            drawn(drag.pane)
                && pane_modes(frame, drag.pane)
                    .is_none_or(|pane| pane.on_alt_screen == drag.on_alt_screen)
        });
        self.resize_drag = self.resize_drag.filter(|drag| drawn(drag.pane));
        self.mouse_capture = self.mouse_capture.filter(|&(pane, _)| drawn(pane));
        self.hovered_pane = self.hovered_pane.filter(|&pane| drawn(pane));
    }

    /// The position in `pane`'s text that the screen cell `at` names, with a
    /// point outside the pane pulled to its nearest edge so a drag that left the
    /// pane still selects up to it.
    ///
    /// The row is absolute: the frame says which line the pane's top visible row
    /// is, and the `n`-th visible row is that line plus `n`. Absolute lines never
    /// move, so output arriving between the paint and the press cannot shift what
    /// the press names.
    fn text_pos_at(&self, frame: &MouseFrame, pane: PaneId, at: Point) -> Option<GridPos> {
        let (col, row) = pane_cell_clamped(self.frame_layout(frame), pane, at)?;
        let top = pane_modes(frame, pane)?.view_top_row;
        Some(GridPos {
            row: top + u64::from(row),
            col,
        })
    }

    /// Which way the view must scroll for a drag held at `at`: `-1` above the
    /// pane's first row, `1` below its last, and `None` while the pointer is
    /// level with the pane.
    ///
    /// Only the vertical edges scroll. Past the left or right edge there is no
    /// further text to reach, so the highlight clamps to the edge column and
    /// stays put.
    fn edge_scroll_direction(&self, frame: &MouseFrame, pane: PaneId, at: Point) -> Option<i8> {
        let rect = pane_content_rect(self.frame_layout(frame), pane)?;
        let bottom = rect.origin.y + rect.size.rows.saturating_sub(1);
        if at.y < rect.origin.y {
            Some(-1)
        } else if at.y > bottom {
            Some(1)
        } else {
            None
        }
    }

    /// `frame` borrowed for hit-testing, with this viewer's own chrome state.
    fn frame_layout<'a>(&self, frame: &'a MouseFrame) -> koshi_renderer::snapshot::FrameLayout<'a> {
        frame.layout(self.chrome(frame.client.active_tab))
    }
}

/// A `FocusPane` for `pane`, naming `client` so the switch moves that viewer's
/// focus and no other's.
fn focus_pane(client: ClientId, pane: PaneId) -> MouseAction {
    MouseAction::Command(Command::FocusPane(FocusPaneArgs {
        target: FocusTarget::Pane(pane),
        client: Some(client),
    }))
}

/// The pane a hit-tested `region` sits in, or `None` when it is chrome. Only a
/// pane's own content counts as hovering that pane — the wheel scrolls it and
/// the renderer marks its border.
fn pane_under(region: HitRegion) -> Option<PaneId> {
    match region {
        HitRegion::PaneContent { pane_id } => Some(pane_id),
        _ => None,
    }
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
fn border_has_neighbor(frame: &MouseFrame, pane: PaneId, side: Direction) -> bool {
    let Some(rect) = drawn_slot(frame, pane).map(|slot| slot.rect) else {
        return false;
    };
    let gap = frame.session.active_tab.gap;
    frame
        .session
        .active_tab
        .layout_solved
        .iter()
        .filter(|slot| slot.visible && slot.pane_id != pane)
        .any(|slot| {
            let other = slot.rect;
            match side {
                Direction::Right => {
                    other.origin.x == (rect.origin.x + rect.size.cols).saturating_add(gap)
                        && overlaps_rows(rect, other)
                }
                Direction::Left => {
                    (other.origin.x + other.size.cols).saturating_add(gap) == rect.origin.x
                        && overlaps_rows(rect, other)
                }
                Direction::Down => {
                    other.origin.y == (rect.origin.y + rect.size.rows).saturating_add(gap)
                        && overlaps_cols(rect, other)
                }
                Direction::Up => {
                    (other.origin.y + other.size.rows).saturating_add(gap) == rect.origin.y
                        && overlaps_cols(rect, other)
                }
            }
        })
}

/// The box `frame` draws for `pane`, or `None` when the frame shows it nowhere —
/// a pane that closed, was hidden, or sits on a tab this frame is not showing.
fn drawn_slot(frame: &MouseFrame, pane: PaneId) -> Option<&PaneSlot> {
    frame
        .session
        .active_tab
        .layout_solved
        .iter()
        .find(|slot| slot.visible && slot.pane_id == pane)
}

/// Whether two pane boxes cover any of the same rows.
fn overlaps_rows(a: koshi_core::geometry::Rect, b: koshi_core::geometry::Rect) -> bool {
    a.origin.y < b.origin.y + b.size.rows && b.origin.y < a.origin.y + a.size.rows
}

/// Whether two pane boxes cover any of the same columns.
fn overlaps_cols(a: koshi_core::geometry::Rect, b: koshi_core::geometry::Rect) -> bool {
    a.origin.x < b.origin.x + b.size.cols && b.origin.x < a.origin.x + a.size.cols
}

/// The frame's entry for `pane`, or `None` when the frame carried no content for
/// it.
fn pane_modes(frame: &MouseFrame, pane: PaneId) -> Option<&MousePane> {
    frame.panes.iter().find(|entry| entry.id == pane)
}

/// This client's focused pane in `frame` when it is a terminal — the pane an
/// event over chrome falls through to. A plugin pane has no program to answer
/// and no scrollback to move, so it is `None`.
fn focused_terminal_pane(frame: &MouseFrame) -> Option<PaneId> {
    let focused = frame.client.focused_pane?;
    frame
        .session
        .active_tab
        .layout_solved
        .iter()
        .any(|slot| slot.pane_id == focused && matches!(slot.kind, PaneKind::Terminal))
        .then_some(focused)
}

/// A scrollback movement for a vertical tick; a horizontal tick moves no
/// vertical view, so it yields `None`.
fn scroll(pane: PaneId, direction: ScrollDirection, lines: usize) -> Option<MouseAction> {
    vertical(direction).map(|up| MouseAction::Scroll { pane, up, lines })
}

/// `Some(true)` for a wheel up, `Some(false)` for a wheel down, `None` for a
/// horizontal tick.
fn vertical(direction: ScrollDirection) -> Option<bool> {
    match direction {
        ScrollDirection::Up => Some(true),
        ScrollDirection::Down => Some(false),
        ScrollDirection::Left | ScrollDirection::Right => None,
    }
}

/// The command-level name for the clipboard the viewer's `copy.clipboard`
/// setting picks.
fn copy_target(backend: koshi_config::types::ClipboardBackend) -> CopyTarget {
    match backend {
        koshi_config::types::ClipboardBackend::Osc52 => CopyTarget::Osc52,
    }
}

/// Cells the pointer at `to` has moved from `from` toward the grabbed `side`,
/// signed for [`Command::ResizePane`]: positive grows the pane (its border moves
/// outward), negative shrinks it. Left/right borders read the x axis, up/down
/// borders read the y axis; motion on the other axis is ignored.
fn resize_delta(side: Direction, from: Point, to: Point) -> i16 {
    let outward = match side {
        Direction::Right => i32::from(to.x) - i32::from(from.x),
        Direction::Left => i32::from(from.x) - i32::from(to.x),
        Direction::Down => i32::from(to.y) - i32::from(from.y),
        Direction::Up => i32::from(from.y) - i32::from(to.y),
    };
    outward.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The cell `from` reaches when `n` cells of a border move asked for in
/// direction `step` are accepted. The inverse of [`resize_delta`]: a positive
/// `step` grows the pane, which walks a left or up border toward zero and a
/// right or down border away from it. Left/right borders move along x, up/down
/// borders along y.
fn advance_along(side: Direction, from: Point, step: i16, n: u16) -> Point {
    let moved = i32::from(step) * i32::from(n);
    match side {
        Direction::Right => Point {
            x: shift(from.x, moved),
            ..from
        },
        Direction::Left => Point {
            x: shift(from.x, -moved),
            ..from
        },
        Direction::Down => Point {
            y: shift(from.y, moved),
            ..from
        },
        Direction::Up => Point {
            y: shift(from.y, -moved),
            ..from
        },
    }
}

/// `coord` moved `by` cells, saturating at both ends of the cell range, so a
/// border at a viewport edge cannot wrap.
fn shift(coord: u16, by: i32) -> u16 {
    (i32::from(coord) + by).clamp(0, i32::from(u16::MAX)) as u16
}

/// `kind` with its button replaced by `button`. Only a drag or release carries a
/// button koshi re-stamps from the capture; other kinds are returned unchanged.
fn with_button(kind: MouseKind, button: MouseButton) -> MouseKind {
    match kind {
        MouseKind::Drag(_) => MouseKind::Drag(button),
        MouseKind::Release(_) => MouseKind::Release(button),
        other => other,
    }
}
