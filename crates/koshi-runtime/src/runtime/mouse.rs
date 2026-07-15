//! Outer mouse routing: hit-test a click against the client's own frame and act
//! on the region it lands on.
//!
//! Peer of [`crate::runtime::input`] for the pointer. A decoded
//! [`MouseInput`] carries a cell in the client's screen space; the runtime
//! builds that client's current frame, asks [`hit_test`](fn@koshi_renderer::hit_test)
//! what sits under the cell, and turns the answer into a command or a tab-strip
//! scroll. The frame is
//! rebuilt per event so the hit-test always reads the pixels the client sees;
//! its grid buffers are shared by `Arc`, so the rebuild is cheap.
//!
//! What each region does on a left press: a **tab** focuses that tab; a
//! **scroll arrow** peeks the tab strip toward its side; **pane content** or a
//! **stack header** focuses that pane; the bare **tabline** begins a peek-drag.
//! A **drag** then scrolls the strip from its anchor, a **wheel** over the
//! tabline steps it one tab, and a **release** ends the drag. Pane borders and
//! the hint bar are ignored here.
//!
//! Scrolling the tab strip is a per-client view change (it never moves focus or
//! touches session state), so it mutates only the client's
//! [`tabline_offset`](koshi_session::client::Client::tabline_offset) and repaints.

use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandSource, FocusPaneArgs, FocusTabArgs, FocusTarget, TabTarget,
};
use koshi_core::geometry::Point;
use koshi_core::ids::{ClientId, CommandId, PaneId, TabId};
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_renderer::snapshot::RenderSnapshot;
use koshi_renderer::{hit_test, tabline_first_visible, HitRegion};
use koshi_session::client::TablineDragState;

use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::state::Runtime;

/// Cells of horizontal drag that scroll the tab strip by one tab.
const TABLINE_DRAG_STEP: i32 = 6;

impl Runtime {
    /// Route one decoded mouse event from `client_id` against its current frame.
    ///
    /// Only a left press and a wheel read the frame, so only they build a
    /// snapshot; a drag scrolls from its stored anchor, a release clears it, and
    /// buttonless motion (crossterm reports every pointer move) does nothing —
    /// none of those rebuild the frame, so moving the mouse costs nothing.
    pub fn handle_mouse_input(&mut self, client_id: ClientId, mouse: MouseInput) {
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                let Some(snapshot) = self.build_snapshot(client_id) else {
                    return;
                };
                let region = hit_test(&snapshot, mouse.at);
                self.mouse_left_press(client_id, &snapshot, region, mouse.at);
            }
            MouseKind::Scroll(direction) => {
                let Some(snapshot) = self.build_snapshot(client_id) else {
                    return;
                };
                let region = hit_test(&snapshot, mouse.at);
                self.scroll_over_tabline(client_id, &snapshot, region, direction);
            }
            MouseKind::Drag(MouseButton::Left) => self.drag_tabline_to(client_id, mouse.at.x),
            MouseKind::Release(_) => self.end_tabline_drag(client_id),
            MouseKind::Press(_) | MouseKind::Drag(_) | MouseKind::Motion => {}
        }
    }

    /// Act on a left press over `region`.
    fn mouse_left_press(
        &mut self,
        client_id: ClientId,
        snapshot: &RenderSnapshot,
        region: HitRegion,
        at: Point,
    ) {
        match region {
            HitRegion::Tab { tab_id } => self.mouse_focus_tab(client_id, tab_id),
            HitRegion::TablineScrollLeft { to } | HitRegion::TablineScrollRight { to } => {
                self.set_tabline_offset(client_id, Some(to));
            }
            HitRegion::PaneContent { pane_id } | HitRegion::StackHeader { pane_id } => {
                self.mouse_focus_pane(client_id, pane_id);
            }
            HitRegion::Tabline => self.begin_tabline_drag(client_id, snapshot, at.x),
            HitRegion::PaneBorder { .. } | HitRegion::Statusline | HitRegion::None => {}
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

    /// Envelope and dispatch a command attributed to `client_id`'s mouse.
    fn dispatch_mouse(&mut self, client_id: ClientId, command: Command) {
        let envelope = CommandEnvelope::new(
            CommandId::new(),
            CommandSource::mouse(client_id),
            SystemTime::now(),
            command,
        );
        let _ = self.dispatch(envelope);
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

#[cfg(test)]
mod tests;
