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
//!   the program in one pane as a mouse report, and records which buttons that
//!   program holds pressed;
//! - [`write_alternate_scroll_arrows`](Server::write_alternate_scroll_arrows) sends cursor
//!   arrows for the alternate-scroll translation of a wheel tick;
//! - [`drag_resize`](Server::drag_resize) moves one pane border a number of
//!   cells and reports how many cells the pane's size changed.
//!
//! A client that leaves the session gets a release written for every button
//! it still holds (`Server::release_client_mouse_buttons`). Carrying the
//! session out for an image swap writes one for every button any client holds
//! ([`carry_out`](Server::carry_out)). A pane that leaves the session ends the
//! buttons held in it, with no release written.
//!
//! A floating pane move the viewer drew ahead of the session arrives as a
//! command under the id the viewer chose
//! ([`run_client_mouse`](Server::run_client_mouse)).
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
use koshi_core::geometry::{Direction, Point};
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, NamedKey};
use koshi_core::mouse::{is_mouse_kind_reported, MouseAnswer, MouseButton, MouseKind};
use koshi_input::keyboard::encode_key_chord;
use koshi_ipc::protocol::WireMouseAction;
use koshi_terminal::mouse_report::encode_mouse;
use koshi_terminal::state::Screen;

use crate::server::Server;

/// A mouse button whose press the session wrote to a pane's program, and
/// whose release it has not written.
pub(crate) struct HeldMouseButton {
    /// The client whose viewer sent the press.
    client_id: ClientId,
    /// The pane whose program took the press.
    pane_id: PaneId,
    /// The button the press named.
    button: MouseButton,
    /// The pane-local cell that the last press or drag written for this button
    /// named, as the viewer sent it, before it was pulled into the pane's grid.
    last_pane_local_cell: Point,
}

impl Server {
    /// Envelope `mouse_command` under `command_id`, attributed to `client_id`'s
    /// mouse, and dispatch it, returning the runtime's result and the cells a
    /// resize refused at a pane minimum can still take.
    fn dispatch_mouse_command(
        &mut self,
        client_id: ClientId,
        command_id: CommandId,
        mouse_command: Command,
    ) -> (CommandResult, Option<u16>) {
        let mouse_command_envelope = CommandEnvelope::from_parts(
            command_id,
            CommandSource::from_mouse(client_id),
            mouse_command,
        );
        self.dispatch_reporting_spare(mouse_command_envelope)
    }

    /// Dispatch a selection command attributed to `client_id`'s mouse, through
    /// the same dispatch every other mutation takes. The result is dropped.
    pub(crate) fn dispatch_visual(&mut self, client_id: ClientId, visual_command: VisualCommand) {
        let _ = self.dispatch_mouse_command(
            client_id,
            CommandId::new(),
            Command::Visual(visual_command),
        );
    }

    /// Ask for `pane_id`'s `border_side` border to move `requested_cell_count` cells in `resize_step`'s
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
        match self.dispatch_mouse_command(client_id, CommandId::new(), resize_command) {
            (CommandResult::Ok { .. }, _) => Ok(()),
            (_, available_donor_cell_count) => Err(available_donor_cell_count.unwrap_or(0)),
        }
    }

    /// Move `pane_id`'s `border_side` border `requested_cell_count` cells and report how many were
    /// actually taken. `resize_step` is the direction: `1` grows `pane_id`, `-1` shrinks
    /// it.
    ///
    /// The whole distance travels in one [`Command::ResizePane`], which is
    /// asked for `resize_step * requested_cell_count`. A refusal at a pane minimum names the cells
    /// the donating pane can still give, and the next round asks for exactly
    /// those. The layout re-measures that spare from the freshly solved rects
    /// on every call. The rounds stop when one takes the whole remainder, or
    /// when a refusal names a spare that is not below what that round asked
    /// for.
    ///
    /// Each round either takes cells or lowers what the next round asks for.
    /// The returned count never passes `requested_cell_count`.
    ///
    /// A drag of 5 cells into a neighbor with room for 2 returns `2`.
    ///
    /// A floating pane takes one [`Command::ResizePane`], and the returned
    /// count is how many cells its outer size changed on the axis
    /// `border_side` moves along: its width for `Left` and `Right`, its height
    /// for `Up` and `Down`. The count is `0` when the session refuses the
    /// resize. A `40`-column float with `10` columns of room, dragged `15`
    /// cells wider, returns `10`. A `40`-column float pinned at column `20` and
    /// drawn at column `10` of a `50`-column pane area, dragged `3` cells
    /// narrower from its right border, is drawn `37` columns wide at column
    /// `13` and returns `3`, though its right border stays.
    pub fn drag_resize(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        border_side: Direction,
        resize_step: i16,
        requested_cell_count: u16,
    ) -> u16 {
        if let Some(previous_cell_count) =
            self.find_floating_pane_axis_cell_count(client_id, pane_id, border_side)
        {
            let _ = self.ask_border_move(
                client_id,
                pane_id,
                border_side,
                resize_step,
                requested_cell_count,
            );
            return self
                .find_floating_pane_axis_cell_count(client_id, pane_id, border_side)
                .map_or(0, |cell_count| previous_cell_count.abs_diff(cell_count));
        }
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
                // `available_donor_cell_count` is what the donating pane has left above its minimum
                // size, always short of what this round asked for.
                Err(available_donor_cell_count)
                    if available_donor_cell_count < requested_round_cell_count =>
                {
                    requested_round_cell_count = available_donor_cell_count;
                }
                Err(_) => break,
            }
        }
        applied_cell_count
    }

    /// The outer cell count of the floating pane `pane_id` from its last size
    /// solve, on the axis its `border_side` border moves along: its width for
    /// `Left` and `Right`, its height for `Up` and `Down`. `None` when no
    /// session holds `client_id` or `pane_id` is no floating member of that
    /// session.
    ///
    /// A `40x12` float → `Right` `40`, `Down` `12`.
    fn find_floating_pane_axis_cell_count(
        &self,
        client_id: ClientId,
        pane_id: PaneId,
        border_side: Direction,
    ) -> Option<u16> {
        let (_, floating_member) = self
            .get_session_for_client(client_id)?
            .floating_set
            .find_member(pane_id)?;
        Some(match border_side {
            Direction::Left | Direction::Right => floating_member.solved_size.column_count,
            Direction::Up | Direction::Down => floating_member.solved_size.row_count,
        })
    }

    /// Move `client_id`'s koshi scrollback view of `pane_id` by `scroll_line_count`, up into
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
    /// [`PaneSnapshot::view_top_row_index`]:
    /// koshi_renderer::snapshot::PaneSnapshot::view_top_row_index
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
                .saturating_sub(terminal_state.compute_effective_view_offset(scroll_offset) as u64),
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

    /// Hand a `mouse_kind` event with `modifier_flags` held to the program in
    /// `pane_id`, as the mouse report that pane's mode asks for, at the cell
    /// `pane_local_cell` (`(0, 0)` is the top-left content cell).
    ///
    /// The tracking level, the encoding and the grid size are read here, at the
    /// moment of the write: a pane that is gone, and a program that turned
    /// mouse reporting off since the frame the viewer decided from, receive
    /// nothing, and a cell past the pane's last column or row is pulled to it.
    /// On an `80x24` pane, cell `(200, 200)` reports column `80`, row `24`.
    ///
    /// A drag or a release reaches the program only for a button that
    /// `client_id` holds in `pane_id`, and a drag or a release of any other
    /// button writes nothing. The release ends the held button whether or not
    /// the pane takes the report. A press the pane takes holds its button for
    /// `client_id` in `pane_id`. A press or a drag the pane takes for a held
    /// button moves that button's last cell to `pane_local_cell`.
    ///
    /// Example: `client-1` presses left in `pane-7` and the program takes it.
    /// A left drag or release from `client-2` in `pane-7` writes nothing. A
    /// left release from `client-1` at cell `(0, 0)` writes
    /// `ESC [ < 0 ; 1 ; 1 m` under SGR encoding.
    ///
    /// A report the pane takes also clears the session's recovery notice and
    /// drops this client's highlight in that pane. A wheel tick leaves the
    /// highlight standing. A report the pane refuses changes neither.
    pub fn forward_mouse_to_pane(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        mouse_kind: MouseKind,
        modifier_flags: BindingModifierFlags,
        pane_local_cell: Point,
    ) {
        let held_mouse_button_index = match mouse_kind {
            MouseKind::Press(mouse_button)
            | MouseKind::Drag(mouse_button)
            | MouseKind::Release(mouse_button) => {
                self.find_held_mouse_button_index(client_id, pane_id, mouse_button)
            }
            MouseKind::Motion | MouseKind::Scroll(_) => None,
        };
        match (mouse_kind, held_mouse_button_index) {
            (MouseKind::Drag(_) | MouseKind::Release(_), None) => return,
            (MouseKind::Release(_), Some(held_mouse_button_index)) => {
                self.held_mouse_buttons.remove(held_mouse_button_index);
            }
            _ => {}
        }
        if !self.write_mouse_report(pane_id, mouse_kind, modifier_flags, pane_local_cell) {
            return;
        }
        match (mouse_kind, held_mouse_button_index) {
            (MouseKind::Press(pressed_button), None) => {
                self.held_mouse_buttons.push(HeldMouseButton {
                    client_id,
                    pane_id,
                    button: pressed_button,
                    last_pane_local_cell: pane_local_cell,
                });
            }
            (MouseKind::Press(_) | MouseKind::Drag(_), Some(held_mouse_button_index)) => {
                self.held_mouse_buttons[held_mouse_button_index].last_pane_local_cell =
                    pane_local_cell;
            }
            _ => {}
        }
        self.clear_session_recovery_notice_after_pane_input(pane_id);
        // A wheel tick leaves the highlight standing; every other forwarded
        // report — click, drag, motion, release — drops it.
        if !matches!(mouse_kind, MouseKind::Scroll(_)) {
            self.clear_selection_on_pane_input(client_id, pane_id);
        }
    }

    /// Write a release for every mouse button `client_id` holds, in the order
    /// the presses were written, and end those held buttons. Each release goes
    /// out at its button's last cell, pulled into the pane's grid as the grid
    /// stands at this call, with no modifier held, in the mode the pane has at
    /// this call. A mode that reports no release receives nothing. The
    /// client's highlight and the session's recovery notice stay as they are.
    ///
    /// Example: `client-1` pressed left in `pane-7` and dragged to cell
    /// `(3, 1)`. Under SGR encoding, `pane-7` receives `ESC [ < 0 ; 4 ; 2 m`.
    /// A press at cell `(70, 20)` on a `40x12` grid is written at `(39, 11)`;
    /// when the grid has grown to `80x24` by this call, the release is written
    /// at `(70, 20)`.
    pub(crate) fn release_client_mouse_buttons(&mut self, client_id: ClientId) {
        let (released_mouse_buttons, kept_mouse_buttons): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.held_mouse_buttons)
                .into_iter()
                .partition(|held_mouse_button| held_mouse_button.client_id == client_id);
        self.held_mouse_buttons = kept_mouse_buttons;
        self.write_mouse_releases(released_mouse_buttons);
    }

    /// Write a release for every mouse button any client holds, as
    /// [`release_client_mouse_buttons`](Self::release_client_mouse_buttons)
    /// writes one client's, and end every held button.
    pub(crate) fn release_every_mouse_button(&mut self) {
        let released_mouse_buttons = std::mem::take(&mut self.held_mouse_buttons);
        self.write_mouse_releases(released_mouse_buttons);
    }

    /// End every mouse button any client holds in `pane_id`, with no release
    /// written.
    pub(crate) fn remove_held_mouse_buttons_in_pane(&mut self, pane_id: PaneId) {
        self.held_mouse_buttons
            .retain(|held_mouse_button| held_mouse_button.pane_id != pane_id);
    }

    /// Write one release of each entry's button to its pane, at its last cell
    /// with no modifier held, in the order of `released_mouse_buttons`.
    fn write_mouse_releases(&self, released_mouse_buttons: Vec<HeldMouseButton>) {
        for released_mouse_button in released_mouse_buttons {
            self.write_mouse_report(
                released_mouse_button.pane_id,
                MouseKind::Release(released_mouse_button.button),
                BindingModifierFlags::NONE,
                released_mouse_button.last_pane_local_cell,
            );
        }
    }

    /// The index in `held_mouse_buttons` of `button` held by `client_id` in
    /// `pane_id`, or `None` when that button is not held there.
    fn find_held_mouse_button_index(
        &self,
        client_id: ClientId,
        pane_id: PaneId,
        button: MouseButton,
    ) -> Option<usize> {
        self.held_mouse_buttons
            .iter()
            .position(|held_mouse_button| {
                held_mouse_button.client_id == client_id
                    && held_mouse_button.pane_id == pane_id
                    && held_mouse_button.button == button
            })
    }

    /// Write the mouse report for a `mouse_kind` event with `modifier_flags`
    /// held at `pane_local_cell` to the program in `pane_id`, reading the
    /// pane's mode as [`forward_mouse_to_pane`](Self::forward_mouse_to_pane)
    /// states. Returns whether the pane took the bytes: `false` for a pane that
    /// is gone, a mode that does not report `mouse_kind`, or a refused write.
    fn write_mouse_report(
        &self,
        pane_id: PaneId,
        mouse_kind: MouseKind,
        modifier_flags: BindingModifierFlags,
        pane_local_cell: Point,
    ) -> bool {
        let Some((mouse_tracking, mouse_encoding, (row_count, column_count))) = self
            .terminal_engine_by_pane_id
            .get(&pane_id)
            .map(|terminal_engine| {
                let terminal_state = terminal_engine.get_terminal_state();
                (
                    terminal_state.get_mouse_tracking(),
                    terminal_state.get_mouse_encoding(),
                    terminal_state.get_active_grid().get_grid_dimensions(),
                )
            })
        else {
            return false;
        };
        if !is_mouse_kind_reported(mouse_tracking, mouse_kind) {
            return false;
        }
        // A mouse report addresses the program's own grid, whose top-left
        // content cell is `(1, 1)`.
        let column_number = pane_local_cell
            .column
            .min(column_count.saturating_sub(1))
            .saturating_add(1);
        let row_number = pane_local_cell
            .row
            .min(row_count.saturating_sub(1))
            .saturating_add(1);
        let Some(mouse_report_bytes) = encode_mouse(
            mouse_kind,
            modifier_flags,
            column_number,
            row_number,
            mouse_tracking,
            mouse_encoding,
        ) else {
            return false;
        };
        self.get_pty_backend()
            .write_pane_input(pane_id, &mouse_report_bytes)
            .is_ok()
    }

    /// Send `arrow_count` cursor arrow keys to `pane_id` for a wheel tick — the
    /// alternate-scroll (`?1007`) translation. `is_scrolling_up` sends up-arrows, otherwise
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
    pub fn write_alternate_scroll_arrows(
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
                    .then(|| terminal_state.is_application_cursor_keys_enabled())
            })
        else {
            return;
        };
        let arrow_key_bytes = encode_key_chord(
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(arrow_key)),
            is_application_cursor_keys_enabled,
        );
        let mut arrow_key_bytes_to_write = Vec::with_capacity(arrow_count * arrow_key_bytes.len());
        for _ in 0..arrow_count {
            arrow_key_bytes_to_write.extend_from_slice(&arrow_key_bytes);
        }
        if !arrow_key_bytes_to_write.is_empty()
            && self
                .get_pty_backend()
                .write_pane_input(pane_id, &arrow_key_bytes_to_write)
                .is_ok()
        {
            self.clear_session_recovery_notice_after_pane_input(pane_id);
        }
    }

    /// Run one round of mouse actions `client_id`'s viewer decided, in the
    /// order it decided them, then answer the round.
    ///
    /// The session hit-tests nothing here: every action names its own target,
    /// and each one runs against the target it names.
    ///
    /// One answer per round goes to the subscriber that views `client_id`,
    /// carrying `request_id`. It holds one entry per scroll and per border
    /// move, in the order those actions ran, and is empty when the round held
    /// neither. Each entry names the pane it is about, and a border move's
    /// entry also names the side and direction it was asked in.
    ///
    /// A floating pane move runs under the `command_id` it carries and adds no
    /// entry to the answer. A move the session runs makes `client_id`'s next
    /// frames name `command_id` as the last applied floating pane move. A move
    /// the session rejects puts one `PlacementCommandRejected` naming
    /// `command_id` on the same subscriber's queue, ahead of the round's
    /// answer.
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
                    mouse_kind,
                    modifier_flags,
                    pane_local_cell,
                } => {
                    self.forward_mouse_to_pane(
                        client_id,
                        pane_id,
                        mouse_kind,
                        modifier_flags,
                        pane_local_cell,
                    );
                }
                WireMouseAction::AlternateScrollArrows {
                    pane_id,
                    is_scrolling_up,
                    arrow_count,
                } => {
                    self.write_alternate_scroll_arrows(pane_id, is_scrolling_up, arrow_count);
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
                    let _ =
                        self.dispatch_mouse_command(client_id, CommandId::new(), *mouse_command);
                }
                WireMouseAction::MoveFloatingPane {
                    command_id,
                    command_args,
                } => {
                    let (command_result, _) = self.dispatch_mouse_command(
                        client_id,
                        command_id,
                        Command::MoveFloatingPane(command_args),
                    );
                    match command_result {
                        CommandResult::Ok { .. } => {}
                        CommandResult::Rejected { .. } => {
                            let _ = self.send_placement_command_rejection(client_id, command_id);
                        }
                    }
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
        let Some(subscriber_id) = self.find_subscriber_id_by_client_id(client_id) else {
            return;
        };
        self.event_bus
            .try_send_mouse_answer(subscriber_id, request_id, mouse_answers);
    }
}

#[cfg(test)]
mod tests;
