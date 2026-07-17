//! Outer mouse routing: hit-test a click against the client's own frame and act
//! on the region it lands on.
//!
//! Peer of [`crate::runtime::input`] for the pointer. A decoded
//! [`MouseInput`] carries a cell in the client's screen space; the runtime
//! builds that client's current frame, asks [`hit_test`](fn@koshi_renderer::hit_test)
//! what sits under the cell, and turns the answer into a command, a tab-strip
//! scroll, or a mouse report handed to the program in the pane. The frame is
//! rebuilt per event so the hit-test always reads the pixels the client sees;
//! its grid buffers are shared by `Arc`, so the rebuild is cheap.
//!
//! What each region does on a left press: a **tab** focuses that tab; a
//! **scroll arrow** peeks the tab strip toward its side; a **stack header**
//! focuses that pane; **pane content** focuses the pane if the click is not
//! already in the focused one, and otherwise goes through to the program; a
//! **pane border** begins a resize drag; the bare **tabline** begins a
//! peek-drag. A **drag** then moves the grabbed border, scrolls the strip, or
//! goes to the program — whichever gesture the press began; a **wheel** over the
//! tabline steps it one tab; a **release** ends a drag or reaches the program.
//! The hint bar is ignored.
//!
//! Anything koshi does not consume is forwarded to the program in the focused
//! pane, encoded as the mouse report its current mode asked for (see
//! [`encode_mouse`](fn@koshi_terminal::mouse_report::encode_mouse)) — this is
//! what lets a mouse-aware TUI in a pane receive clicks, drags, and motion.
//!
//! A border drag resizes through the same [`Command::ResizePane`] the resize
//! keybinding uses, one cell per cell the pointer crosses, so the border tracks
//! the pointer live. The tracked cell advances only when a resize is accepted:
//! pushing the pointer past a pane's minimum size leaves the border pinned at
//! that limit, and a reverse drag moves it the instant the pointer crosses back.
//!
//! Scrolling the tab strip is a per-client view change (it never moves focus or
//! touches session state), so it mutates only the client's
//! [`tabline_offset`](koshi_session::client::Client::tabline_offset) and repaints.

use std::time::{Instant, SystemTime};

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTabArgs,
    FocusTarget, ResizePaneArgs, TabTarget, VisualCommand,
};
use koshi_core::geometry::{Direction, Point};
use koshi_core::ids::{ClientId, CommandId, PaneId, TabId};
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_layout::mode::LayoutMode;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::RenderSnapshot;
use koshi_renderer::{
    hit_test, pane_content_rect, pane_local_cell, tabline_first_visible, HitRegion,
};
use koshi_session::client::{ClickCount, ResizeDragState, TablineDragState};
use koshi_terminal::mouse_report::{encode_mouse, reports};

use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::selection::CLICK_THRESHOLD;
use crate::runtime::state::Runtime;

/// Cells of horizontal drag that scroll the tab strip by one tab.
const TABLINE_DRAG_STEP: i32 = 6;

impl Runtime {
    /// Route one decoded mouse event from `client_id`.
    ///
    /// Koshi acts on what it owns — a click on a tab, a border, the tab strip —
    /// and hands everything else to the program in the pane under the pointer,
    /// encoded as a mouse report when that program asked for one. A press or
    /// wheel reads the frame to hit-test it; a drag or release consults the
    /// stored drag first; a buttonless move rebuilds nothing unless the focused
    /// pane is in any-motion mode, so an idle mouse still costs nothing.
    pub fn handle_mouse_input(&mut self, client_id: ClientId, mouse: MouseInput, now: Instant) {
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                if let Some((snapshot, region)) = self.frame_hit(client_id, mouse.at) {
                    self.mouse_left_press(client_id, &snapshot, region, mouse, now);
                }
            }
            MouseKind::Scroll(direction) => {
                // Scroll drives koshi's own tab strip; forwarding a wheel tick to
                // the pane is a later step.
                if let Some((snapshot, region)) = self.frame_hit(client_id, mouse.at) {
                    self.scroll_over_tabline(client_id, &snapshot, region, direction);
                }
            }
            MouseKind::Drag(MouseButton::Left) => {
                // A press begins exactly one gesture: a border drag resizes, a
                // tabline drag scrolls, a content drag selects, and any other
                // drag is the program's.
                if self
                    .client_mut(client_id)
                    .is_some_and(|client| client.pending_resize_drag().is_some())
                {
                    self.drag_resize_to(client_id, mouse.at);
                } else if self
                    .client_mut(client_id)
                    .is_some_and(|client| client.tabline_drag().is_some())
                {
                    self.drag_tabline_to(client_id, mouse.at.x);
                } else if self
                    .client_mut(client_id)
                    .is_some_and(|client| client.selection_drag().is_some())
                {
                    self.drag_selection_to(client_id, mouse.at, now);
                } else {
                    self.forward_mouse_to_pane(client_id, mouse);
                }
            }
            MouseKind::Release(_) => {
                // A release that ends a koshi drag is koshi's; any other release
                // belongs to the program under the pointer.
                let ending_drag = self.client_mut(client_id).is_some_and(|client| {
                    client.pending_resize_drag().is_some()
                        || client.tabline_drag().is_some()
                        || client.selection_drag().is_some()
                });
                self.end_tabline_drag(client_id);
                self.end_resize_drag(client_id);
                self.end_selection_drag(client_id);
                if !ending_drag {
                    self.forward_mouse_to_pane(client_id, mouse);
                }
            }
            MouseKind::Press(_) | MouseKind::Drag(_) | MouseKind::Motion => {
                self.forward_mouse_to_pane(client_id, mouse);
            }
        }
    }

    /// Build the client's frame and classify the cell `at` landed on, or `None`
    /// when there is no frame to build.
    fn frame_hit(&mut self, client_id: ClientId, at: Point) -> Option<(RenderSnapshot, HitRegion)> {
        let snapshot = self.build_snapshot(client_id)?;
        let region = hit_test(&snapshot, at);
        Some((snapshot, region))
    }

    /// Act on a left press over `region`.
    fn mouse_left_press(
        &mut self,
        client_id: ClientId,
        snapshot: &RenderSnapshot,
        region: HitRegion,
        mouse: MouseInput,
        now: Instant,
    ) {
        match region {
            HitRegion::Tab { tab_id } => self.mouse_focus_tab(client_id, tab_id),
            HitRegion::TablineScrollLeft { to } | HitRegion::TablineScrollRight { to } => {
                self.set_tabline_offset(client_id, Some(to));
            }
            HitRegion::PaneContent { pane_id } => {
                self.click_pane_content(client_id, pane_id, mouse, now);
            }
            HitRegion::StackHeader { pane_id } => self.mouse_focus_pane(client_id, pane_id),
            HitRegion::PaneBorder { pane_id, side } => {
                // Only a real divider — one with an adjacent pane to resize
                // against — begins a resize. The tab-edge outer frame and the
                // boundary above a collapsed stack header have no neighbor.
                if self.config.mouse.border_resize
                    && self.border_has_neighbor(client_id, pane_id, side)
                {
                    self.begin_resize_drag(client_id, pane_id, side, mouse.at);
                }
            }
            HitRegion::Tabline => self.begin_tabline_drag(client_id, snapshot, mouse.at.x),
            HitRegion::Statusline | HitRegion::None => {}
        }
    }

    /// Whether the border on `side` of `pane_id` is a real divider the client
    /// can drag — one with an adjacent pane to resize against, per the layout
    /// tree. The tab-edge outer frame and the boundary above a collapsed stack
    /// header have no neighbor, so they cannot be dragged; a zoomed view draws
    /// no dividers at all — its visible frame is the outer edge — so nothing is
    /// draggable until the client is back in the tiled view.
    fn border_has_neighbor(&self, client_id: ClientId, pane_id: PaneId, side: Direction) -> bool {
        let Some(session) = self.session_for_client(client_id) else {
            return false;
        };
        let Some(client) = session.clients.get(client_id) else {
            return false;
        };
        if matches!(
            client.layout_mode(client.active_tab()),
            LayoutMode::Fullscreen { .. }
        ) {
            return false;
        }
        let Some(tab) = session.tabs.get(&client.active_tab()) else {
            return false;
        };
        koshi_layout::resize::has_adjacent_border(tab.layout(), pane_id, side)
    }

    /// Route a click on a pane's content: a click on a pane the client has not
    /// focused focuses it; a click on the pane it is already in goes through to
    /// the program when that program asked for mouse events, and otherwise
    /// begins a text selection. A first click focuses, a second acts.
    ///
    /// **A mouse-aware program keeps the mouse.** `vim`, `htop`, and `lazygit`
    /// turn mouse reporting on and act on clicks themselves, so a drag inside
    /// one is theirs; a plain shell asks for nothing, so a drag there is a
    /// selection. Which one it is is read from the pane's live mouse mode at the
    /// moment of the press, so it follows a program turning reporting on and off.
    fn click_pane_content(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        mouse: MouseInput,
        now: Instant,
    ) {
        if Some(pane_id) != self.typed_pane(client_id) {
            self.mouse_focus_pane(client_id, pane_id);
        } else if self.pane_reports_mouse(pane_id, mouse.kind) {
            self.forward_mouse_to_pane(client_id, mouse);
        } else {
            let clicks = self.record_click(client_id, MouseButton::Left, now);
            self.begin_selection_drag(client_id, pane_id, mouse.at, clicks, mouse.mods);
        }
    }

    /// Whether the program in `pane_id` asked to be told about `kind`.
    ///
    /// The cheap check the press path makes before deciding whether a gesture is
    /// the program's or koshi's: it reads the pane's mouse mode alone and builds
    /// no frame.
    fn pane_reports_mouse(&self, pane_id: PaneId, kind: MouseKind) -> bool {
        self.terminal_engines
            .get(&pane_id)
            .is_some_and(|engine| reports(engine.state().mouse_tracking(), kind))
    }

    /// Record a press into this client's run of clicks and report what it makes:
    /// one click, two, or three. See
    /// [`MouseState::press`](koshi_session::client::MouseState::press).
    fn record_click(
        &mut self,
        client_id: ClientId,
        button: MouseButton,
        now: Instant,
    ) -> ClickCount {
        self.client_mut(client_id)
            .map_or(ClickCount::Single, |client| {
                client.mouse_state_mut().press(button, now, CLICK_THRESHOLD)
            })
    }

    /// Hand `mouse` to the program in the pane it belongs to, encoded as the
    /// report that pane's mouse mode asked for.
    ///
    /// A button gesture is captured: the press picks the focused pane under the
    /// pointer, and the drags and release that follow go to that same pane even
    /// as the pointer leaves it, clamped to its edges. A bare move goes to the
    /// focused pane under the pointer. A drag or release with no capture — the
    /// press was koshi's, or it focused nothing — is dropped, so no program ever
    /// sees a release without its press.
    ///
    /// The pane's tracking level is checked before the frame is rebuilt, so a
    /// bare move over a pane in no mouse mode costs nothing.
    fn forward_mouse_to_pane(&mut self, client_id: ClientId, mouse: MouseInput) {
        let captured = self.mouse_capture(client_id);
        // A release ends the capture, whether or not it forwards. Which button
        // released cannot be trusted (some terminals report every release as the
        // left button), so any release clears.
        if matches!(mouse.kind, MouseKind::Release(_)) {
            self.set_mouse_capture(client_id, None);
        }
        let (pane_id, clamp, kind) = match mouse.kind {
            MouseKind::Press(_) | MouseKind::Motion => {
                match self.focused_terminal_pane(client_id) {
                    Some(pane) => (pane, false, mouse.kind),
                    None => return,
                }
            }
            // A captured drag or release is re-stamped with the button its press
            // named — the event's own button is unreliable, so the program sees
            // the same button it saw go down.
            MouseKind::Drag(_) | MouseKind::Release(_) => match captured {
                Some((pane, button)) => (pane, true, with_button(mouse.kind, button)),
                None => return,
            },
            MouseKind::Scroll(_) => return,
        };
        // Cheap gate before any frame rebuild: does the program want this event?
        let Some((tracking, encoding)) = self.terminal_engines.get(&pane_id).map(|engine| {
            (
                engine.state().mouse_tracking(),
                engine.state().mouse_encoding(),
            )
        }) else {
            return;
        };
        if !reports(tracking, kind) {
            return;
        }
        let Some(snapshot) = self.build_snapshot(client_id) else {
            return;
        };
        let Some((col, row)) = pane_cell(&snapshot, pane_id, mouse.at, clamp) else {
            return;
        };
        if let Some(bytes) = encode_mouse(kind, mouse.mods, col, row, tracking, encoding) {
            let _ = self.pty_backend().write(pane_id, &bytes);
            // Capture with the press's own button — reliable, unlike a later
            // drag's or release's.
            if let MouseKind::Press(button) = mouse.kind {
                self.set_mouse_capture(client_id, Some((pane_id, button)));
            }
        }
    }

    /// The client's focused pane when it is a terminal, found without solving the
    /// layout — the cheap check the forward path makes before any real work.
    fn focused_terminal_pane(&self, client_id: ClientId) -> Option<PaneId> {
        let session = self.session_for_client(client_id)?;
        let client = session.clients.get(client_id)?;
        let pane_id = client.focused_pane(client.active_tab())?;
        matches!(session.panes.get(pane_id)?.kind(), PaneKind::Terminal).then_some(pane_id)
    }

    /// The pane and pressed button this client's held mouse gesture is captured
    /// to, if any.
    fn mouse_capture(&self, client_id: ClientId) -> Option<(PaneId, MouseButton)> {
        self.session_for_client(client_id)?
            .clients
            .get(client_id)?
            .mouse_capture()
    }

    /// Set or clear this client's mouse capture.
    fn set_mouse_capture(&mut self, client_id: ClientId, capture: Option<(PaneId, MouseButton)>) {
        if let Some(client) = self.client_mut(client_id) {
            client.set_mouse_capture(capture);
        }
    }

    /// Dispatch a `FocusTab` for a clicked tab. The client is named explicitly,
    /// so the switch moves this client's view and no other.
    fn mouse_focus_tab(&mut self, client_id: ClientId, tab_id: TabId) {
        self.dispatch_mouse(
            client_id,
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Id(tab_id),
                client: Some(client_id),
            }),
        );
    }

    /// Dispatch a `FocusPane` for a clicked pane.
    fn mouse_focus_pane(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.dispatch_mouse(
            client_id,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(pane_id),
                client: Some(client_id),
            }),
        );
    }

    /// Envelope and dispatch a command attributed to `client_id`'s mouse,
    /// returning the runtime's result.
    fn dispatch_mouse_command(&mut self, client_id: ClientId, command: Command) -> CommandResult {
        let envelope = CommandEnvelope::new(
            CommandId::new(),
            CommandSource::mouse(client_id),
            SystemTime::now(),
            command,
        );
        self.dispatch(envelope)
    }

    /// Envelope and dispatch a command attributed to `client_id`'s mouse.
    fn dispatch_mouse(&mut self, client_id: ClientId, command: Command) {
        let _ = self.dispatch_mouse_command(client_id, command);
    }

    /// Dispatch a selection command attributed to `client_id`'s mouse. The
    /// selection layer's route into the command pipeline, so a highlight lands
    /// through the same dispatch every other mutation does.
    pub(crate) fn dispatch_visual(&mut self, client_id: ClientId, command: VisualCommand) {
        self.dispatch_mouse(client_id, Command::Visual(command));
    }

    /// Anchor a tab-strip peek-drag at column `anchor_x`, recording the first
    /// visible tab at that instant so drag motion scrolls relative to it.
    fn begin_tabline_drag(
        &mut self,
        client_id: ClientId,
        snapshot: &RenderSnapshot,
        anchor_x: u16,
    ) {
        let anchor_first_visible = tabline_first_visible(snapshot);
        if let Some(client) = self.client_mut(client_id) {
            client.set_tabline_drag(Some(TablineDragState {
                anchor_x,
                anchor_first_visible,
            }));
        }
    }

    /// Scroll the tab strip to follow an in-flight drag whose pointer is now at
    /// column `x`. Dragging right moves the strip right (revealing earlier
    /// tabs); one tab per [`TABLINE_DRAG_STEP`] cells.
    fn drag_tabline_to(&mut self, client_id: ClientId, x: u16) {
        let Some(client) = self.client_mut(client_id) else {
            return;
        };
        let Some(drag) = client.tabline_drag() else {
            return;
        };
        let delta = i32::from(drag.anchor_x) - i32::from(x);
        let steps = delta / TABLINE_DRAG_STEP;
        let target = (drag.anchor_first_visible as i32 + steps).max(0) as usize;
        client.set_tabline_offset(Some(target));
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
    }

    /// End any in-flight tab-strip drag, leaving the scrolled position as it is.
    fn end_tabline_drag(&mut self, client_id: ClientId) {
        if let Some(client) = self.client_mut(client_id) {
            if client.tabline_drag().is_some() {
                client.set_tabline_drag(None);
            }
        }
    }

    /// Begin a pane-border resize drag: record the grabbed pane, its border
    /// `side`, and the press cell `at` that the first drag move measures from.
    fn begin_resize_drag(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        side: Direction,
        at: Point,
    ) {
        if let Some(client) = self.client_mut(client_id) {
            client.update_pending_resize_drag(Some(ResizeDragState {
                pane: pane_id,
                side,
                last: at,
            }));
        }
    }

    /// Move the grabbed border to follow a drag whose pointer is now at `at`.
    ///
    /// Applies the move one cell at a time toward the border, through the same
    /// [`Command::ResizePane`] path the resize keybinding uses, so a fast drag
    /// that jumps several cells fills right up to a pane's minimum size instead
    /// of being refused whole. The tracked cell advances only over the cells that
    /// were accepted; the first refused step is the wall, so the border pins
    /// there and a reverse drag moves it the instant the pointer crosses back.
    fn drag_resize_to(&mut self, client_id: ClientId, at: Point) {
        let Some(drag) = self
            .client_mut(client_id)
            .and_then(|client| client.pending_resize_drag().copied())
        else {
            return;
        };
        let total = resize_delta(drag.side, drag.last, at);
        if total == 0 {
            return;
        }
        // One cell per step, in the pointer's direction, stopping at the first
        // refused step — that is the wall, and every further step this way fails
        // too, so the anchor stays on the cells that actually moved.
        let step = total.signum();
        let mut applied: u16 = 0;
        for _ in 0..total.unsigned_abs() {
            let command = Command::ResizePane(ResizePaneArgs {
                pane: Some(drag.pane),
                direction: drag.side,
                size: step,
            });
            if !matches!(
                self.dispatch_mouse_command(client_id, command),
                CommandResult::Ok { .. }
            ) {
                break;
            }
            applied += 1;
        }
        if applied > 0 {
            if let Some(client) = self.client_mut(client_id) {
                let last = advance_toward(drag.side, drag.last, at, applied);
                client.update_pending_resize_drag(Some(ResizeDragState { last, ..drag }));
            }
        }
    }

    /// End any in-flight pane-border resize drag.
    fn end_resize_drag(&mut self, client_id: ClientId) {
        if let Some(client) = self.client_mut(client_id) {
            if client.pending_resize_drag().is_some() {
                client.update_pending_resize_drag(None);
            }
        }
    }

    /// Step the tab strip one tab on a wheel `direction`, but only when the
    /// wheel is over the tabline row.
    fn scroll_over_tabline(
        &mut self,
        client_id: ClientId,
        snapshot: &RenderSnapshot,
        region: HitRegion,
        direction: ScrollDirection,
    ) {
        let over_tabline = matches!(
            region,
            HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. }
        );
        if !over_tabline {
            return;
        }
        let first = tabline_first_visible(snapshot);
        let target = match direction {
            ScrollDirection::Up | ScrollDirection::Left => first.saturating_sub(1),
            ScrollDirection::Down | ScrollDirection::Right => first + 1,
        };
        self.set_tabline_offset(client_id, Some(target));
    }

    /// Set this client's tab-strip peek offset and repaint. The renderer clamps
    /// the index to a valid window, so an over-far target is harmless.
    fn set_tabline_offset(&mut self, client_id: ClientId, offset: Option<usize>) {
        let Some(client) = self.client_mut(client_id) else {
            return;
        };
        client.set_tabline_offset(offset);
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
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

/// The cell `n` cells from `from` toward `to` along `side`'s axis — where the
/// drag anchor lands after `n` accepted single-cell resizes. Moving toward the
/// pointer keeps the anchor correct for both a grow and a shrink. Saturating, so
/// a border at a viewport edge cannot wrap below zero.
fn advance_toward(side: Direction, from: Point, to: Point, n: u16) -> Point {
    match side {
        Direction::Left | Direction::Right => Point {
            x: step_toward(from.x, to.x, n),
            ..from
        },
        Direction::Up | Direction::Down => Point {
            y: step_toward(from.y, to.y, n),
            ..from
        },
    }
}

/// The 1-based cell inside `pane_id` that `at` maps to. With `clamp`, a point
/// outside the pane is pulled to its nearest edge, so a captured drag that left
/// the pane still selects to the edge; without it, an outside point is [`None`].
fn pane_cell(
    snapshot: &RenderSnapshot,
    pane_id: PaneId,
    at: Point,
    clamp: bool,
) -> Option<(u16, u16)> {
    if !clamp {
        return pane_local_cell(snapshot, pane_id, at);
    }
    let rect = pane_content_rect(snapshot, pane_id)?;
    let right = rect.origin.x + rect.size.cols.saturating_sub(1);
    let bottom = rect.origin.y + rect.size.rows.saturating_sub(1);
    let x = at.x.clamp(rect.origin.x, right);
    let y = at.y.clamp(rect.origin.y, bottom);
    Some((x - rect.origin.x + 1, y - rect.origin.y + 1))
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

/// `from` moved `n` cells toward `to`, saturating at zero.
fn step_toward(from: u16, to: u16, n: u16) -> u16 {
    if to >= from {
        from.saturating_add(n)
    } else {
        from.saturating_sub(n)
    }
}

#[cfg(test)]
mod tests;
