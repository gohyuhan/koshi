//! Selection command handlers — the commands of visual mode.

use super::*;

impl Server {
    /// Route a [`Command::Visual`] sub-command to its handler.
    ///
    /// Every variant acts on the issuing client's own highlights: a highlight
    /// belongs to one client, and a gone issuer takes its highlights with it
    /// ([`Self::issuing_client`]). [`Self::validate`] has already confirmed the
    /// source names a client.
    pub(super) fn handle_visual(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        command: &VisualCommand,
    ) -> Result<CommandResult, Rejection> {
        match command {
            VisualCommand::SetSelection(args) => {
                self.handle_set_selection(command_id, source, args)
            }
            VisualCommand::ClearSelection(args) => {
                self.handle_clear_selection(command_id, source, args)
            }
            VisualCommand::Copy(args) => self.handle_copy(command_id, source, args),
        }
    }

    /// Handle [`VisualCommand::SetSelection`]: highlight `args.selection` in
    /// `args.pane` for the issuing client, replacing any highlight it had there.
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
        source: &CommandSource,
        args: &SetSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::issuing_client(source)?;
        self.require_pane(client_id, args.pane)?;
        let selection = self.snapped(args.pane, args.selection);
        let client = self
            .client_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        client.set_selection(args.pane, selection);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id: args.pane,
                selection: Some(selection),
            })],
        ))
    }

    /// `selection` with each end pulled onto the cell its glyph really lives in,
    /// and then — for a word or line selection — grown outward to whole words or
    /// whole lines. A pane with no terminal text comes back untouched.
    fn snapped(&self, pane_id: PaneId, selection: Selection) -> Selection {
        let Some(engine) = self.terminal_engines.get(&pane_id) else {
            return selection;
        };
        let view = engine.state().text_view();
        let anchor = glyph_cell(&view, selection.anchor);
        let cursor = glyph_cell(&view, selection.cursor);
        // Which end leads decides which way each one grows.
        let forward = (anchor.row, anchor.col) <= (cursor.row, cursor.col);
        let (first, last) = if forward {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        let (first, last) = match selection.kind {
            // A character or block highlight covers the cells the pointer named.
            SelectionKind::Character | SelectionKind::Block => (first, last),
            SelectionKind::Word => {
                let (row, col) = view.word_start(first.row, first.col);
                let start = GridPos { row, col };
                let (row, col) = view.word_end(last.row, last.col);
                (start, GridPos { row, col })
            }
            SelectionKind::Line => (
                GridPos {
                    row: view.line_start(first.row),
                    col: 0,
                },
                GridPos {
                    row: view.line_end(last.row),
                    col: view.cols().saturating_sub(1),
                },
            ),
        };
        let (anchor, cursor) = if forward {
            (first, last)
        } else {
            (last, first)
        };
        Selection {
            anchor,
            cursor,
            ..selection
        }
    }

    /// Handle [`VisualCommand::Copy`]: put the issuing client's highlight in
    /// `args.pane` on the clipboard, leaving the highlight standing.
    ///
    /// The text is read at this instant from the pane's own lines, not from
    /// what is on screen, so a highlight running off the top of the view copies
    /// whole. `args.trim_trailing_whitespace` drops the blanks a terminal
    /// pads each row out to the pane's width with: a highlight over `hello` in an
    /// 80-column pane copies `hello` when it is set, and `hello` plus 75 blanks
    /// when it is not. `args.target` says which clipboard receives it.
    ///
    /// A pane with no highlight, or one whose highlight covers no text, copies
    /// nothing and is not an error.
    pub(super) fn handle_copy(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &CopyArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::issuing_client(source)?;
        self.require_pane(client_id, args.pane)?;
        let selection = self
            .client_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?
            .selection(args.pane);
        let text = selection
            .zip(self.terminal_engines.get(&args.pane))
            .map(|(selection, engine)| {
                koshi_terminal::selection::selection_text(
                    &engine.state().text_view(),
                    &selection,
                    args.trim_trailing_whitespace,
                )
            })
            .unwrap_or_default();
        if !text.is_empty() {
            self.copy_to_clipboard(client_id, args.target, &text);
        }
        Ok(Self::commit_events(&mut self.event_bus, command_id, vec![]))
    }

    /// Handle [`VisualCommand::ClearSelection`]: drop the issuing client's
    /// highlight in `args.pane`, leaving visual mode for that pane.
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
        source: &CommandSource,
        args: &ClearSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::issuing_client(source)?;
        self.require_pane(client_id, args.pane)?;
        let client = self
            .client_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        client.clear_selection(args.pane);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id: args.pane,
                selection: None,
            })],
        ))
    }
}

/// `pos` moved onto the cell its glyph really occupies.
///
/// A wide (CJK or emoji) glyph fills two columns: its text lives in the left
/// one and the right one is a width-0 cell the renderer never paints. A pointer
/// on either half names the glyph itself, so a highlight can never cover only an
/// invisible cell. `世界` at columns 0–3 with the pointer on column 1 yields
/// column 0.
fn glyph_cell(view: &koshi_terminal::selection::TextView<'_>, pos: GridPos) -> GridPos {
    let mut col = pos.col;
    while col > 0
        && (view
            .cell(pos.row, col)
            .is_some_and(|cell| cell.width() == 0)
            || view.is_wide_wrap_spacer(pos.row, col))
    {
        col -= 1;
    }
    GridPos { col, ..pos }
}
