//! Selection command handlers — the commands of visual mode.

use super::*;

impl Server {
    /// Route a [`Command::Visual`] sub-command to its handler.
    ///
    /// Every variant acts on the issuing client's own highlights — a highlight
    /// belongs to one client, so there is no other client it could mean, and a
    /// gone issuer takes its highlights with it rather than falling back to
    /// another client ([`Self::issuing_client`]). [`Self::validate`] has
    /// already confirmed the source names a client.
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
            // The copy surface for commands (IPC, plugins) is unbuilt; the
            // interactive copy happens at the selection gesture's release.
            VisualCommand::Copy(_) => Ok(self.reject(command_id, "copy")),
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
    /// A pane that does not exist in the client's session is
    /// [`RejectReason::TargetGone`] — the drag that named it raced a close.
    pub(super) fn handle_set_selection(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &SetSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::issuing_client(source)?;
        self.require_pane(client_id, args.pane)?;
        let client = self
            .client_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        client.set_selection(args.pane, args.selection);
        Ok(Self::commit_events(
            &mut self.event_bus,
            command_id,
            vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id: args.pane,
                selection: Some(args.selection),
            })],
        ))
    }

    /// Handle [`VisualCommand::ClearSelection`]: drop the issuing client's
    /// highlight and matching in-flight drag in `args.pane`, ending selection
    /// activity for that pane.
    ///
    /// Clearing a pane with neither state changes nothing and is not an error:
    /// the ways selection ends (a click, a key press) fire without first
    /// checking whether either was active.
    ///
    /// Dropping the highlight releases the hold it had on the view, so a view at
    /// the live bottom follows new output again. A view that had also been
    /// scrolled up stays held by the offset.
    pub(super) fn handle_clear_selection(
        &mut self,
        command_id: CommandId,
        source: &CommandSource,
        args: &ClearSelectionArgs,
    ) -> Result<CommandResult, Rejection> {
        let client_id = Self::issuing_client(source)?;
        let client = self
            .client_mut(client_id)
            .ok_or_else(|| Rejection::bare(RejectReason::SourceClientStale))?;
        client.clear_selection(args.pane);
        if client
            .selection_drag()
            .is_some_and(|drag| drag.pane == args.pane)
        {
            client.set_selection_drag(None);
        }
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
