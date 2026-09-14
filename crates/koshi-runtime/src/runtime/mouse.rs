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
//! - [`drag_resize`](Server::drag_resize) moves one pane border a number of
//!   cells and reports how many it took.
//!
//! Focus and every selection change arrive as ordinary commands through
//! [`Server::submit_command`], validated like any command typed at the CLI.
//!
//! An out-of-process viewer sends a whole round of these at once, which
//! [`run_client_mouse`](Server::run_client_mouse) runs in order and answers.
//!
//! **Each call re-reads the live state it needs at the moment it acts**: the
//! pane's mouse mode, its active screen and its minimum size are taken as they
//! stand at the call, not as the frame the viewer decided from had them.

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ResizePaneArgs, VisualCommand,
};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use koshi_core::mouse::{is_mouse_kind_reported, MouseAnswer, MouseInput, MouseKind};
use koshi_input::keyboard::encode_key_chord;
use koshi_ipc::protocol::WireMouseAction;
use koshi_renderer::compute_clamped_pane_cell;
use koshi_renderer::snapshot::ViewerChrome;
use koshi_terminal::mouse_report::encode_mouse;
use koshi_terminal::state::Screen;

use std::time::SystemTime;

use crate::server::Server;

impl Server {
    /// Envelope and dispatch a command attributed to `client_id`'s mouse,
    /// returning the runtime's result and the cells a resize refused at a pane
    /// minimum can still take.
    fn dispatch_mouse_command(
        &mut self,
        client_id: ClientId,
        mouse_command: Command,
    ) -> (CommandResult, Option<u16>) {
        let mouse_command_envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_mouse(client_id),
            SystemTime::now(),
            mouse_command,
        );
        self.dispatch_reporting_spare(mouse_command_envelope)
    }

    /// Dispatch a selection command attributed to `client_id`'s mouse, through
    /// the same dispatch every other mutation takes. The result is dropped.
    pub(crate) fn dispatch_visual(&mut self, client_id: ClientId, visual_command: VisualCommand) {
        let _ = self.dispatch_mouse_command(client_id, Command::Visual(visual_command));
    }

    /// Ask for `pane`'s `side` border to move `cells` cells in `step`'s
    /// direction. `Err` carries the cells the donating pane can still give: `0`
    /// when it is already at its minimum size, and `0` for every rejection that
    /// is not a minimum-size refusal.
    fn ask_border_move(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        border_side: Direction,
        resize_step: i16,
        requested_cell_count: u16,
    ) -> Result<(), u16> {
        // `resize_step * requested_cell_count` outside ±`i16::MAX` is clamped to it.
        let resize_cell_count = (i32::from(resize_step) * i32::from(requested_cell_count))
            .clamp(-i32::from(i16::MAX), i32::from(i16::MAX))
            as i16;
        let resize_command = Command::ResizePane(ResizePaneArgs {
            pane_id: Some(pane_id),
            direction: border_side,
            resize_amount_cells: resize_cell_count,
        });
        match self.dispatch_mouse_command(client_id, resize_command) {
            (CommandResult::Ok { .. }, _) => Ok(()),
            (_, available_cell_count) => Err(available_cell_count.unwrap_or(0)),
        }
    }

    /// Move `pane`'s `side` border `count` cells and report how many were
    /// actually taken. `step` is the direction: `1` grows `pane`, `-1` shrinks
    /// it.
    ///
    /// The whole distance travels in one [`Command::ResizePane`], which is
    /// asked for `step * cells`. A refusal at a pane minimum names the cells
    /// the donating pane can still give, and the next round asks for exactly
    /// those. The layout re-measures that spare from the freshly solved rects
    /// on every call. The rounds stop when one takes the whole remainder, or
    /// when a refusal names a spare that is not below what that round asked
    /// for.
    ///
    /// Each round either takes cells or lowers what the next round asks for.
    /// `applied` never passes `count`.
    ///
    /// A drag of 5 cells into a neighbor with room for 2 returns `2`.
    pub fn drag_resize(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        border_side: Direction,
        resize_step: i16,
        requested_cell_count: u16,
    ) -> u16 {
        let mut applied_cell_count: u16 = 0;
        // What this round asks for: the whole remaining distance, or the cells
        // the last round was told the donating pane can still give.
        let mut requested_round_cell_count = requested_cell_count;
        while requested_round_cell_count > 0 {
            match self.ask_border_move(
                client_id,
                pane_id,
                border_side,
                resize_step,
                requested_round_cell_count,
            ) {
                Ok(()) => {
                    applied_cell_count =
                        applied_cell_count.saturating_add(requested_round_cell_count);
                    requested_round_cell_count =
                        requested_cell_count.saturating_sub(applied_cell_count);
                }
                // `available_cell_count` is what the donating pane has left above its minimum
                // size, always short of what this round asked for.
                Err(available_cell_count) if available_cell_count < requested_round_cell_count => {
                    requested_round_cell_count = available_cell_count;
                }
                Err(_) => break,
            }
        }
        applied_cell_count
    }

    /// Move `client_id`'s koshi scrollback view of `pane_id` by `lines`, up into
    /// history or back down toward live output, and report the line its top row
    /// now shows.
    ///
    /// Koshi scrollback exists only on the primary screen: a pane on the
    /// alternate screen scrolls nothing, and the alternate screen keeps no
    /// history. The screen is read here, at the moment of the move, not as the
    /// frame the viewer decided from had it.
    ///
    /// The returned line is the same number [`PaneSnapshot::view_top_row_index`] would
    /// carry for the next frame. `None` names a pane with no terminal.
    ///
    /// [`PaneSnapshot::view_top_row_index`]: koshi_renderer::snapshot::PaneSnapshot::view_top_row_index
    pub fn scroll_pane_view(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        is_scrolling_up: bool,
        scroll_line_count: usize,
    ) -> Option<u64> {
        if self.is_pane_on_primary_screen(pane_id) {
            if is_scrolling_up {
                self.scroll_up(client_id, pane_id, scroll_line_count);
            } else {
                self.scroll_down(client_id, pane_id, scroll_line_count);
            }
        }
        self.get_view_top_row_index(client_id, pane_id)
    }

    /// The line `client_id`'s view of `pane_id` shows on its top row, or `None`
    /// when the pane has no terminal.
    fn get_view_top_row_index(&self, client_id: ClientId, pane_id: PaneId) -> Option<u64> {
        let scroll_offset = self
            .get_session_for_client(client_id)
            .and_then(|session| session.clients.get_client_by_id(client_id))
            .map_or(0, |attached_client| {
                attached_client.get_scroll_offset(pane_id)
            });
        let terminal_state = self
            .terminal_engine_by_pane_id
            .get(&pane_id)?
            .get_terminal_state();
        Some(
            terminal_state
                .get_scrollback()
                .get_total_pushed_line_count()
                .saturating_sub(terminal_state.effective_view_offset(scroll_offset) as u64),
        )
    }

    /// Whether `pane_id`'s program is on the primary screen — the only screen
    /// with koshi scrollback to scroll.
    fn is_pane_on_primary_screen(&self, pane_id: PaneId) -> bool {
        self.terminal_engine_by_pane_id
            .get(&pane_id)
            .is_some_and(|terminal_engine| {
                terminal_engine.get_terminal_state().get_active_screen() == Screen::Primary
            })
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
    /// An event that reaches the pane's writer also drops this client's
    /// highlight in that pane, whether the write succeeds or fails. A wheel
    /// tick leaves the highlight standing.
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
        mouse_input: MouseInput,
    ) -> bool {
        let Some((tracking, encoding)) =
            self.terminal_engine_by_pane_id
                .get(&pane_id)
                .map(|terminal_engine| {
                    let terminal_state = terminal_engine.get_terminal_state();
                    (
                        terminal_state.get_mouse_tracking(),
                        terminal_state.get_mouse_encoding(),
                    )
                })
        else {
            return false;
        };
        if !is_mouse_kind_reported(tracking, mouse_input.mouse_kind) {
            return false;
        }
        let Some(owned_frame_layout) = self.build_frame_layout(client_id) else {
            return false;
        };
        // A mouse report addresses the program's own grid, whose top-left
        // content cell is `(1, 1)`.
        let Some((column_index, row_index)) = compute_clamped_pane_cell(
            owned_frame_layout.build_frame_layout(ViewerChrome::default()),
            pane_id,
            mouse_input.position,
        )
        .map(|(column_index, row_index)| (column_index + 1, row_index + 1)) else {
            return false;
        };
        let Some(mouse_report_bytes) = encode_mouse(
            mouse_input.mouse_kind,
            mouse_input.modifier_flags,
            column_index,
            row_index,
            tracking,
            encoding,
        ) else {
            return false;
        };
        let is_report_written = self
            .get_pty_backend()
            .write_pane_input(pane_id, &mouse_report_bytes)
            .is_ok();
        // A wheel tick leaves the highlight standing; every other forwarded
        // report — click, drag, motion, release — drops it.
        if !matches!(mouse_input.mouse_kind, MouseKind::Scroll(_)) {
            self.clear_selection_on_pane_input(client_id, pane_id);
        }
        is_report_written
    }

    /// Send `arrow_count` cursor arrow keys to `pane_id` for a wheel tick — the
    /// alternate-scroll (`?1007`) translation. `up` sends up-arrows, otherwise
    /// down-arrows.
    ///
    /// The pane must still be on the alternate screen with alternate scroll on,
    /// read here at the moment of the write: a pane whose program left the
    /// alternate screen since the frame the viewer decided from receives
    /// nothing.
    ///
    /// The byte form follows the program's cursor-key mode (DECCKM), read at the
    /// same moment: `ESC O A` under application keys, `ESC [ A` otherwise.
    ///
    /// An `arrow_count` of `0` writes nothing.
    pub fn write_alt_scroll_arrows(
        &mut self,
        pane_id: PaneId,
        is_scrolling_up: bool,
        arrow_count: usize,
    ) {
        let arrow_key = if is_scrolling_up {
            NamedKey::Up
        } else {
            NamedKey::Down
        };
        let Some(is_application_cursor_keys_enabled) = self
            .terminal_engine_by_pane_id
            .get(&pane_id)
            .and_then(|terminal_engine| {
                let terminal_state = terminal_engine.get_terminal_state();
                (terminal_state.is_alternate_scroll_enabled()
                    && terminal_state.get_active_screen() == Screen::Alternate)
                    .then(|| terminal_state.are_application_cursor_keys_enabled())
            })
        else {
            return;
        };
        let arrow_key_bytes = encode_key_chord(
            KeyChord::from_parts(ModFlags::NONE, Key::Named(arrow_key)),
            is_application_cursor_keys_enabled,
        );
        let mut arrow_key_bytes_to_write = Vec::with_capacity(arrow_count * arrow_key_bytes.len());
        for _ in 0..arrow_count {
            arrow_key_bytes_to_write.extend_from_slice(&arrow_key_bytes);
        }
        if !arrow_key_bytes_to_write.is_empty() {
            let _ = self
                .get_pty_backend()
                .write_pane_input(pane_id, &arrow_key_bytes_to_write);
        }
    }

    /// Run one round of mouse actions `client_id`'s viewer decided, in the
    /// order it decided them, then answer the round.
    ///
    /// The session hit-tests nothing here: every action names its own target,
    /// and each one runs against the target it names.
    ///
    /// One answer per round goes to the subscriber that views `client_id`,
    /// carrying `request_id`. It holds one entry per action that had
    /// something to report — a scroll and a border move — in the order those
    /// actions ran, and is empty when none did. Each entry names the pane it is
    /// about, and a border move's entry also names the side and direction it
    /// was asked in.
    pub fn run_client_mouse(
        &mut self,
        client_id: ClientId,
        request_id: u64,
        mouse_actions: Vec<WireMouseAction>,
    ) {
        let mut mouse_answers = Vec::new();
        for mouse_action in mouse_actions {
            match mouse_action {
                WireMouseAction::Scroll {
                    pane_id,
                    is_scrolling_up,
                    scroll_line_count,
                } => {
                    let view_top_row_index = self.scroll_pane_view(
                        client_id,
                        pane_id,
                        is_scrolling_up,
                        scroll_line_count,
                    );
                    mouse_answers.push(MouseAnswer::Scrolled {
                        pane_id,
                        top_row_number: view_top_row_index,
                    });
                }
                WireMouseAction::Forward {
                    pane_id,
                    mouse_input,
                } => {
                    let _ = self.forward_mouse_to_pane(client_id, pane_id, mouse_input);
                }
                WireMouseAction::AltScrollArrows {
                    pane_id,
                    is_scrolling_up,
                    arrow_count,
                } => {
                    self.write_alt_scroll_arrows(pane_id, is_scrolling_up, arrow_count);
                }
                WireMouseAction::Resize {
                    pane_id,
                    border_side,
                    resize_step,
                    requested_cell_count,
                } => {
                    let applied_cell_count = self.drag_resize(
                        client_id,
                        pane_id,
                        border_side,
                        resize_step,
                        requested_cell_count,
                    );
                    mouse_answers.push(MouseAnswer::Resized {
                        pane_id,
                        border_side,
                        resize_step,
                        applied_cell_count,
                    });
                }
                WireMouseAction::Command(mouse_command) => {
                    let _ = self.dispatch_mouse_command(client_id, *mouse_command);
                }
            }
        }
        self.answer_mouse_round(client_id, request_id, mouse_answers);
    }

    /// Put `mouse_answers` on the queue of the subscriber that views `client_id`, as
    /// the answer to mouse round `request_id`.
    ///
    /// A client with no subscription has nothing queued. A subscriber whose
    /// queue is full, which is paused, or whose receiver is gone loses the
    /// answer.
    fn answer_mouse_round(
        &mut self,
        client_id: ClientId,
        request_id: u64,
        mouse_answers: Vec<MouseAnswer>,
    ) {
        let Some(&(subscriber_id, _)) = self
            .subscriptions
            .iter()
            .find(|&&(_, viewed_client_id)| viewed_client_id == client_id)
        else {
            return;
        };
        self.event_bus
            .try_send_answer(subscriber_id, request_id, mouse_answers);
    }
}

#[cfg(test)]
mod tests;
