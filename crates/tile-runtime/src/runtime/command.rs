//! Command dispatch: the single entrypoint every requested mutation passes
//! through.
//!
//! [`Runtime::dispatch`] takes one [`CommandEnvelope`] and returns one
//! [`CommandResult`] via an exhaustive `match` on [`Command`] — one arm per
//! variant. Handlers do not exist yet (they land in later command tasks), so
//! every arm currently rejects cleanly with [`RejectReason::InvalidState`] and
//! a diagnostic hint. The exhaustive match is the point: a new `Command`
//! variant cannot be added without giving it an arm here, and each handler
//! replaces its arm in place as it ships.

use tile_core::{
    command::{Command, CommandEnvelope, CommandResult},
    event::RejectReason,
    ids::CommandId,
};

use crate::runtime::state::Runtime;

impl Runtime {
    /// Dispatch one command and report its outcome.
    ///
    /// Every mutation enters here; nothing mutates session, layout, or pane
    /// state outside a handler reached through this method. With no handlers
    /// wired yet, each command is rejected with [`RejectReason::InvalidState`].
    pub fn dispatch(&mut self, envelope: CommandEnvelope) -> CommandResult {
        match envelope.command {
            Command::NewPane(_) => self.reject(envelope.id, "new pane"),
            Command::ClosePane(_) => self.reject(envelope.id, "close pane"),
            Command::ResizePane(_) => self.reject(envelope.id, "resize pane"),
            Command::FocusPane(_) => self.reject(envelope.id, "focus pane"),
            Command::NewTab(_) => self.reject(envelope.id, "new tab"),
            Command::CloseTab(_) => self.reject(envelope.id, "close tab"),
            Command::RenameTab(_) => self.reject(envelope.id, "rename tab"),
            Command::FocusTab(_) => self.reject(envelope.id, "focus tab"),
            Command::WriteToPane(_) => self.reject(envelope.id, "write to pane"),
            Command::ToggleLockMode => self.reject(envelope.id, "toggle lock mode"),
            Command::SetLockMode(_) => self.reject(envelope.id, "set lock mode"),
            Command::RunCommandPane(_) => self.reject(envelope.id, "run command pane"),
            Command::CopyMode(_) => self.reject(envelope.id, "copy mode"),
            Command::Plugin(_) => self.reject(envelope.id, "plugin"),
            Command::TogglePaneFullscreen => self.reject(envelope.id, "toggle pane fullscreen"),
            Command::RenamePane(_) => self.reject(envelope.id, "rename pane"),
            Command::MoveTab(_) => self.reject(envelope.id, "move tab"),
            Command::RenameSession(_) => self.reject(envelope.id, "rename session"),
        }
    }

    /// Build a rejection for a command with no handler wired yet, keyed back to
    /// its originating envelope by `command_id`. `label` names the command in
    /// the human-facing hint.
    fn reject(&self, command_id: CommandId, label: &str) -> CommandResult {
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!("{label} not yet implemented")),
        }
    }
}

#[cfg(test)]
mod tests;
