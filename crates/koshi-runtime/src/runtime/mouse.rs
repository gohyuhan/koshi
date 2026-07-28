//! What the session does with a mouse event the viewer already decided.
//!
//! The viewer that received the event answers it against the frame it last
//! painted: which pane the pointer is over, which region it landed on, which
//! gesture is under way. Nothing here hit-tests, and no mouse event arrives
//! here raw. What reaches the session is one of these calls, each naming its
//! target explicitly:
//!
//! - [`scroll_pane_view`](Server::scroll_pane_view) moves a client's scrollback
//!   view of one pane;
//! - [`forward_mouse_to_pane`](Server::forward_mouse_to_pane) hands an event to
//!   the program in one pane as a mouse report;
//! - [`write_alt_scroll_arrows`](Server::write_alt_scroll_arrows) sends cursor
//!   arrows for the alternate-scroll translation of a wheel tick;
//! - [`drag_resize`](Server::drag_resize) moves one pane border a cell at a
//!   time and reports how far it got.
//!
//! Focus and every selection change arrive as ordinary commands through
//! [`Server::submit_command`], validated like any command typed at the CLI.
//!
//! **Each call re-reads the live state it needs at the moment it acts**, so a
//! program that changed a mouse mode, switched screens, or hit its minimum size
//! since the frame the viewer decided from is still answered correctly.

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ResizePaneArgs, VisualCommand,
};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::mouse::{reports, MouseInput, MouseKind};
use koshi_renderer::pane_cell_clamped;
use koshi_renderer::snapshot::ViewerChrome;
use koshi_terminal::mouse_report::encode_mouse;
use koshi_terminal::state::Screen;

use std::time::SystemTime;

use crate::server::Server;

impl Server {
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

    /// Dispatch a selection command attributed to `client_id`'s mouse. The
    /// selection layer's route into the command pipeline, so a highlight lands
    /// through the same dispatch every other mutation does.
    pub(crate) fn dispatch_visual(&mut self, client_id: ClientId, command: VisualCommand) {
        let _ = self.dispatch_mouse_command(client_id, Command::Visual(command));
    }

    /// Move `pane`'s `side` border `count` cells, `step` at a time, and report
    /// how many cells were actually taken.
    ///
    /// Each cell goes through the same [`Command::ResizePane`] the resize
    /// keybinding uses, so a fast drag that jumps several cells fills right up
    /// to a pane's minimum size. The first refused step is the wall: every
    /// further step that way fails too, so the walk stops there and the count
    /// names the cells that really moved.
    ///
    /// A drag of 5 cells into a neighbor with room for 2 returns `2`.
    pub fn drag_resize(
        &mut self,
        client_id: ClientId,
        pane: PaneId,
        side: Direction,
        step: i16,
        count: u16,
    ) -> u16 {
        let mut applied = 0;
        for _ in 0..count {
            let command = Command::ResizePane(ResizePaneArgs {
                pane: Some(pane),
                direction: side,
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
        applied
    }

    /// Move `client_id`'s koshi scrollback view of `pane_id` by `lines`, up into
    /// history or back down toward live output, and report the line its top row
    /// now shows.
    ///
    /// Koshi scrollback exists only on the primary screen, so a pane on the
    /// alternate screen scrolls nothing: the alternate screen keeps no history,
    /// and storing an offset there would surface as an unexpectedly scrolled-back
    /// shell once the full-screen program exits back to the primary. The screen
    /// is read here, at the moment of the move, so a pane that switched screens
    /// since the frame the viewer decided from is still answered correctly.
    ///
    /// The returned line is the same number [`PaneSnapshot::view_top_row`] would
    /// carry for the next frame, so a caller can tell a view that moved from one
    /// already at its limit. `None` names a pane with no terminal.
    ///
    /// [`PaneSnapshot::view_top_row`]: koshi_renderer::snapshot::PaneSnapshot::view_top_row
    pub fn scroll_pane_view(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        up: bool,
        lines: usize,
    ) -> Option<u64> {
        if self.pane_on_primary(pane_id) {
            if up {
                self.scroll_up(client_id, pane_id, lines);
            } else {
                self.scroll_down(client_id, pane_id, lines);
            }
        }
        self.view_top_row(client_id, pane_id)
    }

    /// The line `client_id`'s view of `pane_id` shows on its top row, or `None`
    /// when the pane has no terminal.
    fn view_top_row(&self, client_id: ClientId, pane_id: PaneId) -> Option<u64> {
        let offset = self
            .session_for_client(client_id)
            .and_then(|session| session.clients.get(client_id))
            .map_or(0, |client| client.scroll_offset(pane_id));
        let state = self.terminal_engines.get(&pane_id)?.state();
        Some(
            state
                .scrollback()
                .total_pushed()
                .saturating_sub(state.effective_view_offset(offset) as u64),
        )
    }

    /// Whether `pane_id`'s program is on the primary screen — the only screen
    /// with koshi scrollback to scroll.
    fn pane_on_primary(&self, pane_id: PaneId) -> bool {
        self.terminal_engines
            .get(&pane_id)
            .is_some_and(|engine| engine.state().active_screen() == Screen::Primary)
    }

    /// Hand `mouse` to the program in `pane_id`, encoded as the mouse report
    /// that pane's mode asks for.
    ///
    /// The tracking level and encoding are read here, at the moment of the
    /// write, so a program that turned mouse reporting off since the frame the
    /// viewer decided from receives nothing.
    ///
    /// The pointer's cell is clamped into the pane, so an event that landed on
    /// chrome (a border, the status line) or left the pane mid-drag still
    /// reaches it at the nearest edge.
    ///
    /// An event that is written also drops this client's highlight in that pane:
    /// input reaching the pane's child leaves visual mode.
    pub fn forward_mouse_to_pane(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        mouse: MouseInput,
    ) {
        let Some((tracking, encoding)) = self.terminal_engines.get(&pane_id).map(|engine| {
            (
                engine.state().mouse_tracking(),
                engine.state().mouse_encoding(),
            )
        }) else {
            return;
        };
        if !reports(tracking, mouse.kind) {
            return;
        }
        let Some(frame) = self.build_layout(client_id) else {
            return;
        };
        // A mouse report addresses the program's own grid, whose top-left
        // content cell is `(1, 1)`.
        let Some((col, row)) =
            pane_cell_clamped(frame.layout(ViewerChrome::default()), pane_id, mouse.at)
                .map(|(col, row)| (col + 1, row + 1))
        else {
            return;
        };
        if let Some(bytes) = encode_mouse(mouse.kind, mouse.mods, col, row, tracking, encoding) {
            let _ = self.pty_backend().write(pane_id, &bytes);
            // A wheel tick is not input the program's child typed, so it leaves
            // a highlight standing; a click, drag, or release is.
            if !matches!(mouse.kind, MouseKind::Scroll(_)) {
                self.clear_selection_on_pane_input(client_id, pane_id);
            }
        }
    }

    /// Send `count` cursor arrow keys to `pane_id` for a wheel tick — the
    /// alternate-scroll (`?1007`) translation. `up` sends up-arrows, otherwise
    /// down-arrows.
    ///
    /// The pane must still be on the alternate screen with alternate scroll on,
    /// read here at the moment of the write: a pane whose program left the
    /// alternate screen since the frame the viewer decided from receives
    /// nothing, so the arrows cannot reach the shell underneath and recall its
    /// history.
    ///
    /// The byte form follows the program's cursor-key mode (DECCKM), read at the
    /// same moment: `ESC O A` under application keys, `ESC [ A` otherwise.
    pub fn write_alt_scroll_arrows(&mut self, pane_id: PaneId, up: bool, count: usize) {
        let letter = if up { b'A' } else { b'B' };
        let Some(app_keys) = self.terminal_engines.get(&pane_id).and_then(|engine| {
            let state = engine.state();
            (state.alt_scroll() && state.active_screen() == Screen::Alternate)
                .then(|| state.app_cursor_keys())
        }) else {
            return;
        };
        let intro: &[u8] = if app_keys { b"\x1bO" } else { b"\x1b[" };
        let mut bytes = Vec::with_capacity(count * 3);
        for _ in 0..count {
            bytes.extend_from_slice(intro);
            bytes.push(letter);
        }
        if !bytes.is_empty() {
            let _ = self.pty_backend().write(pane_id, &bytes);
        }
    }
}

#[cfg(test)]
mod tests;
