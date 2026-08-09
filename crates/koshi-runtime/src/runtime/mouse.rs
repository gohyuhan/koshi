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
//! An out-of-process viewer sends a whole round of these at once, which
//! [`run_client_mouse`](Server::run_client_mouse) runs in order and answers.
//!
//! **Each call re-reads the live state it needs at the moment it acts**, so a
//! program that changed a mouse mode, switched screens, or hit its minimum size
//! since the frame the viewer decided from is still answered correctly.

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ResizePaneArgs, VisualCommand,
};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::mouse::{reports, MouseAnswer, MouseInput, MouseKind};
use koshi_ipc::protocol::WireMouseAction;
use koshi_renderer::pane_cell_clamped;
use koshi_renderer::snapshot::ViewerChrome;
use koshi_terminal::mouse_report::encode_mouse;
use koshi_terminal::state::Screen;

use std::time::SystemTime;

use crate::server::Server;

impl Server {
    /// Envelope and dispatch a command attributed to `client_id`'s mouse,
    /// returning the runtime's result.
    /// Envelope and dispatch a command attributed to `client_id`'s mouse,
    /// returning the runtime's result and the cells a resize refused at a pane
    /// minimum can still take.
    fn dispatch_mouse_command(
        &mut self,
        client_id: ClientId,
        command: Command,
    ) -> (CommandResult, Option<u16>) {
        let envelope = CommandEnvelope::new(
            CommandId::new(),
            CommandSource::mouse(client_id),
            SystemTime::now(),
            command,
        );
        self.dispatch_reporting_spare(envelope)
    }

    /// Dispatch a selection command attributed to `client_id`'s mouse. The
    /// selection layer's route into the command pipeline, so a highlight lands
    /// through the same dispatch every other mutation does.
    pub(crate) fn dispatch_visual(&mut self, client_id: ClientId, command: VisualCommand) {
        let _ = self.dispatch_mouse_command(client_id, Command::Visual(command));
    }

    /// Ask for `pane`'s `side` border to move `cells` cells in `step`'s
    /// direction. `Err` carries the cells the donating pane can still give,
    /// which is `0` when it is already at its minimum size.
    fn ask_border_move(
        &mut self,
        client_id: ClientId,
        pane: PaneId,
        side: Direction,
        step: i16,
        cells: u16,
    ) -> Result<(), u16> {
        // Clamping to ±i16::MAX keeps the edge-flip's `saturating_neg`
        // symmetric.
        let size = (i32::from(step) * i32::from(cells))
            .clamp(-i32::from(i16::MAX), i32::from(i16::MAX)) as i16;
        let command = Command::ResizePane(ResizePaneArgs {
            pane: Some(pane),
            direction: side,
            size,
        });
        match self.dispatch_mouse_command(client_id, command) {
            (CommandResult::Ok { .. }, _) => Ok(()),
            (_, spare) => Err(spare.unwrap_or(0)),
        }
    }

    /// Move `pane`'s `side` border `count` cells and report how many were
    /// actually taken.
    ///
    /// The whole distance travels in one [`Command::ResizePane`]. A refusal at
    /// a pane minimum names the cells the donating pane can still give, and the
    /// next round asks for exactly those, so a drag fills right up to the
    /// minimum. The layout re-measures that spare from the freshly solved rects
    /// on every call, so the rounds keep going until one takes the whole
    /// remainder or the layout offers nothing.
    ///
    /// Each round either takes cells or lowers what the next round asks for, so
    /// the walk ends and `applied` never passes `count`.
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
        let mut applied: u16 = 0;
        // What this round asks for: the whole remaining distance, or the cells
        // the last round was told the donating pane can still give.
        let mut ask = count;
        while ask > 0 {
            match self.ask_border_move(client_id, pane, side, step, ask) {
                Ok(()) => {
                    applied = applied.saturating_add(ask);
                    ask = count.saturating_sub(applied);
                }
                // `spare` is what the donating pane has left above its minimum
                // size, always short of what this round asked for.
                Err(spare) if spare < ask => ask = spare,
                Err(_) => break,
            }
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
    ///
    /// Returns whether a report was handed to the pane's writer. It is `false`
    /// when the pane is gone, when its live tracking no longer asks for this
    /// event, when the layout no longer places the pane, and when the pane
    /// refuses the bytes — so the caller records a gesture only for a press the
    /// pane accepted.
    pub fn forward_mouse_to_pane(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        mouse: MouseInput,
    ) -> bool {
        let Some((tracking, encoding)) = self.terminal_engines.get(&pane_id).map(|engine| {
            (
                engine.state().mouse_tracking(),
                engine.state().mouse_encoding(),
            )
        }) else {
            return false;
        };
        if !reports(tracking, mouse.kind) {
            return false;
        }
        let Some(frame) = self.build_layout(client_id) else {
            return false;
        };
        // A mouse report addresses the program's own grid, whose top-left
        // content cell is `(1, 1)`.
        let Some((col, row)) =
            pane_cell_clamped(frame.layout(ViewerChrome::default()), pane_id, mouse.at)
                .map(|(col, row)| (col + 1, row + 1))
        else {
            return false;
        };
        let Some(bytes) = encode_mouse(mouse.kind, mouse.mods, col, row, tracking, encoding) else {
            return false;
        };
        let written = self.pty_backend().write(pane_id, &bytes).is_ok();
        // A wheel tick is not input the program's child typed, so it leaves
        // a highlight standing; a click, drag, or release is.
        if !matches!(mouse.kind, MouseKind::Scroll(_)) {
            self.clear_selection_on_pane_input(client_id, pane_id);
        }
        written
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

    /// Run one round of mouse actions `client_id`'s viewer decided, in the
    /// order it decided them, then answer the round.
    ///
    /// The session hit-tests nothing here: every action names its own target,
    /// so this walks the list and calls the door each one names.
    ///
    /// One answer is queued for the round, always, carrying `request_id`. It
    /// holds one entry per action that had something to report — a scroll and a
    /// border move — in the order those actions ran, and is empty when none
    /// did. Each entry names the pane it is about, and a border move's entry
    /// also names the side and direction it was asked in, so several moves in
    /// one round stay told apart.
    pub fn run_client_mouse(
        &mut self,
        client_id: ClientId,
        request_id: u64,
        actions: Vec<WireMouseAction>,
    ) {
        let mut answers = Vec::new();
        for action in actions {
            match action {
                WireMouseAction::Scroll { pane, up, lines } => {
                    let top = self.scroll_pane_view(client_id, pane, up, lines);
                    answers.push(MouseAnswer::Scrolled { pane, top });
                }
                WireMouseAction::Forward { pane, mouse } => {
                    let _ = self.forward_mouse_to_pane(client_id, pane, mouse);
                }
                WireMouseAction::AltScrollArrows { pane, up, count } => {
                    self.write_alt_scroll_arrows(pane, up, count);
                }
                WireMouseAction::Resize {
                    pane,
                    side,
                    step,
                    count,
                } => {
                    let applied = self.drag_resize(client_id, pane, side, step, count);
                    answers.push(MouseAnswer::Resized {
                        pane,
                        side,
                        step,
                        applied,
                    });
                }
                WireMouseAction::Command(command) => {
                    let _ = self.dispatch_mouse_command(client_id, *command);
                }
            }
        }
        self.answer_mouse_round(client_id, request_id, answers);
    }

    /// Put `answers` on the queue of the subscriber that views `client_id`, as
    /// the answer to mouse round `request_id`.
    ///
    /// A client with no subscription is no attached viewer, so it is waiting on
    /// nothing and nothing is queued.
    fn answer_mouse_round(
        &mut self,
        client_id: ClientId,
        request_id: u64,
        answers: Vec<MouseAnswer>,
    ) {
        let Some(&(subscriber, _)) = self
            .subscriptions
            .iter()
            .find(|&&(_, viewed)| viewed == client_id)
        else {
            return;
        };
        self.event_bus
            .try_send_answer(subscriber, request_id, answers);
    }
}

#[cfg(test)]
mod tests;
