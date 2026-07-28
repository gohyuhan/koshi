//! What a wheel tick means, decided by the viewer that received it.
//!
//! A viewer holds its own `mouse` settings — how many lines a notch scrolls,
//! and what the wheel does over a plain pane — so two viewers of one session
//! answer the same tick differently. It also holds a [`MouseFrame`] of the
//! frame it last painted, which says where every surface sits and which mouse
//! modes each pane's program had. That is everything a wheel needs, so the
//! viewer decides and the session only executes.
//!
//! What the viewer does **not** decide is how a forwarded tick is encoded, or
//! how far a scroll may actually travel. It names a pane and a movement; the
//! session re-reads that pane's live modes and its retained history at the
//! moment it writes.
//!
//! **The frame is one event old.** A program that flips a mouse mode between
//! the last paint and this tick is answered from the old modes once, and the
//! next frame corrects it. A forwarded tick is never wrong even then: the
//! session drops it when the live modes no longer ask for it.

use koshi_config::types::WheelScroll;
use koshi_core::ids::PaneId;
use koshi_core::mouse::{reports, MouseInput, MouseKind, ScrollDirection};
use koshi_renderer::snapshot::{MouseFrame, PaneKind};
use koshi_renderer::{hit_test, tabline_first_visible, HitRegion};

use crate::Client;

#[cfg(test)]
mod tests;

/// What the viewer decided one wheel tick means: where the pointer is, and what
/// the session must do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelDecision {
    /// The pane under the pointer, or `None` over koshi's own chrome. The
    /// session marks it hovered so the renderer can color the wheel's target.
    pub hovered: Option<PaneId>,
    /// What to do, or `None` when this tick does nothing — a horizontal wheel
    /// where only a vertical one acts, or a plain pane under a viewer whose
    /// `mouse.wheel` setting is `ignore`.
    pub action: Option<MouseAction>,
}

/// One thing the viewer wants the session to do for a wheel tick. Every variant
/// names its target explicitly; the session hit-tests nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Hand the tick to the program in `pane` as a mouse report. The session
    /// encodes it from that pane's live tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane: PaneId,
        /// The tick, with the cell it landed on and the modifiers held.
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
    /// Scroll this client's tab strip so tab index `to` is the first visible
    /// one. A per-client view change; it never moves focus.
    ScrollTabline {
        /// The tab index the strip scrolls to.
        to: usize,
    },
}

impl Client {
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
    #[must_use]
    pub fn handle_mouse_wheel(
        &self,
        mouse: MouseInput,
        frame: &MouseFrame,
    ) -> Option<WheelDecision> {
        let MouseKind::Scroll(direction) = mouse.kind else {
            return None;
        };
        let region = hit_test(frame.layout(), mouse.at);
        if let Some(to) = tabline_step(frame, region, direction) {
            return Some(WheelDecision {
                hovered: None,
                action: Some(MouseAction::ScrollTabline { to }),
            });
        }
        let hovered = match region {
            HitRegion::PaneContent { pane_id } => Some(pane_id),
            _ => None,
        };
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
        let modes = frame.panes.iter().find(|entry| entry.id == pane)?;
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
}

/// The tab index a wheel tick over the tab strip scrolls to, or `None` when the
/// tick did not land on the strip. Up and left step toward the first tab, down
/// and right toward the last.
fn tabline_step(
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
    let first = tabline_first_visible(frame.layout());
    Some(match direction {
        ScrollDirection::Up | ScrollDirection::Left => first.saturating_sub(1),
        ScrollDirection::Down | ScrollDirection::Right => first + 1,
    })
}

/// This client's focused pane in `frame` when it is a terminal — the pane a
/// wheel over chrome falls through to. A plugin pane has no program to answer
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
