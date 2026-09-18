//! Selection command handlers — the commands of visual mode.

use super::*;
use koshi_core::command::GridPosition;

impl Server {
    /// Route a [`Command::Visual`] sub-command to its handler.
    ///
    /// Every variant acts on the issuing client's own highlights: a highlight
    /// belongs to one client, and a gone issuer takes its highlights with it
    /// ([`Self::resolve_issuing_client_id`]). [`Self::validate_command`] has already confirmed the
    /// command source names a client.
    pub(super) fn handle_visual(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        visual_command: &VisualCommand,
    ) -> Result<CommandResult, Rejection> {
        match visual_command {
            VisualCommand::SetSelection(command_args) => {
                self.handle_set_selection(command_id, command_source, command_args)
            }
            VisualCommand::ClearSelection(command_args) => {
                self.handle_clear_selection(command_id, command_source, command_args)
            }
            VisualCommand::Copy(command_args) => {
                self.handle_copy(command_id, command_source, command_args)
            }
        }
    }

    /// Handle [`VisualCommand::SetSelection`]: highlight `command_args.selection` in
    /// `command_args.pane_id` for the issuing client, replacing any highlight it had there.
    ///
    /// Only this client's highlight in this one pane moves — its highlights in
    /// other panes, and every other client's, are untouched. Highlighting also
    /// holds this client's view of the pane, so output arriving underneath
    /// cannot drag the highlighted text off the screen
    /// ([`Client::is_view_held`]).
    ///
    /// **A word or line highlight is grown here, not by the caller.** The
    /// pointer names two cells. Both ends grow away from each other, so the
    /// pair always covers the text between them however the drag runs, and
    /// re-growing an already-grown pair changes nothing.
    ///
    /// `hello world` with a word drag from the `e` of `hello` to the `o` of
    /// `world`: the stored anchor falls back to the `h` and the stored cursor
    /// runs on to the `d`, giving `hello world` entire. Character and block
    /// highlights are stored exactly as they arrive — they mean the cells the
    /// pointer named.
    ///
    /// A pane that does not exist in the client's session is
    /// [`RejectReason::TargetGone`].
    pub(super) fn handle_set_selection(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &SetSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::resolve_issuing_client_id(command_source)?;
        self.validate_pane_exists(client_id, command_args.pane_id)?;
        let selection = self.snap_selection(command_args.pane_id, command_args.selection);
        let client = self
            .get_client_mut(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        client.set_selection(command_args.pane_id, selection);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id: command_args.pane_id,
                selection: Some(selection),
            })],
        ))
    }

    /// `selection` with each end pulled onto the cell its glyph really lives in,
    /// and then — for a word or line selection — grown outward to whole words or
    /// whole lines. A pane with no terminal text comes back untouched.
    fn snap_selection(&self, pane_id: PaneId, selection: Selection) -> Selection {
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get(&pane_id) else {
            return selection;
        };
        let text_view = terminal_engine.get_terminal_state().get_text_view();
        let anchor = compute_glyph_cell(&text_view, selection.anchor);
        let cursor = compute_glyph_cell(&text_view, selection.cursor);
        // Which end leads decides which way each one grows.
        let is_forward_selection =
            (anchor.row_index, anchor.column_index) <= (cursor.row_index, cursor.column_index);
        let (first_grid_position, last_grid_position) = if is_forward_selection {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        let (first_grid_position, last_grid_position) = match selection.selection_kind {
            // A character or block highlight covers the cells the pointer named.
            SelectionKind::Character | SelectionKind::Block => {
                (first_grid_position, last_grid_position)
            }
            SelectionKind::Word => {
                let (row_index, column_index) = text_view.get_word_start_position(
                    first_grid_position.row_index,
                    first_grid_position.column_index,
                );
                let start_grid_position = GridPosition {
                    row_index,
                    column_index,
                };
                let (row_index, column_index) = text_view.get_word_end_position(
                    last_grid_position.row_index,
                    last_grid_position.column_index,
                );
                (
                    start_grid_position,
                    GridPosition {
                        row_index,
                        column_index,
                    },
                )
            }
            SelectionKind::Line => (
                GridPosition {
                    row_index: text_view.get_line_start_row_index(first_grid_position.row_index),
                    column_index: 0,
                },
                GridPosition {
                    row_index: text_view.get_line_end_row_index(last_grid_position.row_index),
                    column_index: text_view.get_column_count().saturating_sub(1),
                },
            ),
        };
        let (anchor, cursor) = if is_forward_selection {
            (first_grid_position, last_grid_position)
        } else {
            (last_grid_position, first_grid_position)
        };
        Selection {
            anchor,
            cursor,
            ..selection
        }
    }

    /// Handle [`VisualCommand::Copy`]: put the issuing client's highlight in
    /// `command_args.pane_id` on the clipboard, leaving the highlight standing.
    ///
    /// The text is read at this instant from the pane's own lines, not from
    /// what is on screen, so a highlight running off the top of the view copies
    /// whole. `command_args.should_trim_trailing_whitespace` drops the blanks a terminal
    /// pads each row out to the pane's width with: a highlight over `hello` in an
    /// 80-column pane copies `hello` when it is set, and `hello` plus 75 blanks
    /// when it is not. `command_args.clipboard_target` says which clipboard receives it.
    ///
    /// A pane with no highlight, or one whose highlight covers no text, copies
    /// nothing and is not an error.
    pub(super) fn handle_copy(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &CopyArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::resolve_issuing_client_id(command_source)?;
        self.validate_pane_exists(client_id, command_args.pane_id)?;
        let selection = self
            .get_client_mut(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?
            .get_selection(command_args.pane_id);
        let copied_text = selection
            .zip(self.terminal_engine_by_pane_id.get(&command_args.pane_id))
            .map(|(selection, terminal_engine)| {
                koshi_terminal::selection::serialize_selection_text(
                    &terminal_engine.get_terminal_state().get_text_view(),
                    &selection,
                    command_args.should_trim_trailing_whitespace,
                )
            })
            .unwrap_or_default();
        if !copied_text.is_empty() {
            self.copy_to_clipboard(client_id, command_args.clipboard_target, &copied_text);
        }
        Ok(Self::commit_events(&mut self.event_bus, command_id, vec![]))
    }

    /// Handle [`VisualCommand::ClearSelection`]: drop the issuing client's
    /// highlight in `command_args.pane_id`, leaving visual mode for that pane.
    ///
    /// Clearing a pane with no highlight changes nothing and is not an error.
    ///
    /// Dropping the highlight releases the hold it had on the view, so a view at
    /// the live bottom follows new output again. A view that had also been
    /// scrolled up stays held by the offset.
    ///
    /// A pane that does not exist in the client's session is
    /// [`RejectReason::TargetGone`].
    pub(super) fn handle_clear_selection(
        &mut self,
        command_id: CommandId,
        command_source: &CommandSource,
        command_args: &ClearSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::resolve_issuing_client_id(command_source)?;
        self.validate_pane_exists(client_id, command_args.pane_id)?;
        let client = self
            .get_client_mut(client_id)
            .ok_or_else(|| Rejection::from_reason(RejectReason::SourceClientStale))?;
        client.clear_selection(command_args.pane_id);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id: command_args.pane_id,
                selection: None,
            })],
        ))
    }
}

/// `grid_position` moved onto the cell its glyph really occupies.
///
/// A wide (CJK or emoji) glyph fills two columns: its text lives in the left
/// one and the right one is a width-0 cell the renderer never paints. A pointer
/// on either half names the glyph itself, so a highlight can never cover only an
/// invisible cell. `世界` at columns 0–3 with the pointer on column 1 yields
/// column 0.
fn compute_glyph_cell(
    text_view: &koshi_terminal::selection::TextView<'_>,
    grid_position: GridPosition,
) -> GridPosition {
    let mut column_index = grid_position.column_index;
    while column_index > 0
        && (text_view
            .get_cell(grid_position.row_index, column_index)
            .is_some_and(|grid_cell| grid_cell.get_display_width() == 0)
            || text_view.is_wide_wrap_spacer(grid_position.row_index, column_index))
    {
        column_index -= 1;
    }
    GridPosition {
        column_index,
        ..grid_position
    }
}
