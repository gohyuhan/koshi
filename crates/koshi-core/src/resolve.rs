//! Action resolution — turning a named action into the plan that runs it.
//!
//! [`action`](crate::action) ships the vocabulary and [`registry`](crate::registry)
//! holds the built-in table. This module ships the one step between them and
//! the dispatcher: given an [`ActionReference`] such as `core:next-tab`, produce
//! the [`Command`] the runtime should execute.
//!
//! # Actions and commands are not one-to-one
//!
//! Several actions build the same command and differ only by a value fixed for
//! that action: `lock` and `unlock` both build [`Command::SetLockMode`],
//! `next-tab` and `previous-tab` both build [`Command::FocusTab`]. Those fixed
//! values live in a table keyed on the action's NAME. The registry record's
//! [`CommandKind`](crate::command::CommandKind) is not read.
//!
//! # What this module does not decide
//!
//! Every command it builds names its target as `None`, which each argument
//! struct reads as "the focused one". Resolution is a pure function of the
//! reference, the registry, the caller's own split direction, and the
//! caller's scroll line count: the caller passes in its
//! `layout.new-pane-direction`, and `core:new-pane` builds its split with it.
//!
//! # Routes
//!
//! A registry entry's [`ActionHandlerReference`] picks one of two plans. A
//! [`CoreCommand`](ActionHandlerReference::CoreCommand) builds a typed command. A
//! [`CoreClient`](ActionHandlerReference::CoreClient) returns a viewer-local action.

use std::fmt;

use crate::action::{ActionHandlerReference, ActionReference, ClientActionKind};
use crate::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    NewPaneArgs, NewPanePlacement, NewTabArgs, ResizePaneArgs, ScrollPaneArgs, TabTarget,
    ToggleLockModeArgs,
};
use crate::geometry::Direction;
use crate::registry::ActionRegistry;

/// The number of lines a scroll action scrolls when the caller names no count.
pub const DEFAULT_SCROLL_LINE_COUNT: u16 = 3;

/// What running one action amounts to: one command or one viewer-local action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchPlan {
    /// Dispatch one typed command.
    Command(Box<Command>),
    /// Run one viewer-local action without sending a session command.
    ClientAction(ClientActionKind),
}

/// Why an action could not be turned into a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The reference names no registry record.
    Unregistered {
        /// The reference that was not found.
        action_reference: ActionReference,
    },
    /// The action builds a command that needs arguments only its CLI verb
    /// supplies, such as `core:run` (a program) or `core:move-pane` (a
    /// direction).
    ArgumentsRequired {
        /// The reference that needs arguments.
        action_reference: ActionReference,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Unregistered { action_reference } => {
                write!(formatter, "action {action_reference} is not registered")
            }
            ResolveError::ArgumentsRequired { action_reference } => {
                write!(
                    formatter,
                    "action {action_reference} needs arguments only its CLI verb supplies"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Turn an action reference into the plan that runs it.
///
/// `new_pane_direction` is the caller's own `layout.new-pane-direction`
/// setting. `core:new-pane` builds its split with it.
/// The scroll actions scroll [`DEFAULT_SCROLL_LINE_COUNT`] lines.
///
/// # Errors
/// - [`ResolveError::Unregistered`] if `action_reference` names no registry record in `registry`.
/// - [`ResolveError::ArgumentsRequired`] if `action_reference` builds a command
///   that needs arguments.
pub fn resolve_action(
    action_reference: &ActionReference,
    registry: &ActionRegistry,
    new_pane_direction: Direction,
) -> Result<DispatchPlan, ResolveError> {
    resolve_action_with_scroll_line_count(
        action_reference,
        registry,
        new_pane_direction,
        DEFAULT_SCROLL_LINE_COUNT,
    )
}

/// Turn an action into a plan, with `scroll_line_count` as the line count of
/// `core:scroll-pane-up` and `core:scroll-pane-down`.
///
/// [`resolve_action`] passes [`DEFAULT_SCROLL_LINE_COUNT`]. A client passes the
/// count its mouse configuration names. Errors as [`resolve_action`].
pub fn resolve_action_with_scroll_line_count(
    action_reference: &ActionReference,
    registry: &ActionRegistry,
    new_pane_direction: Direction,
    scroll_line_count: u16,
) -> Result<DispatchPlan, ResolveError> {
    let action_metadata = registry
        .find_action_metadata(action_reference)
        .ok_or_else(|| ResolveError::Unregistered {
            action_reference: action_reference.clone(),
        })?;

    match action_metadata.handler {
        ActionHandlerReference::CoreCommand(_) => {
            resolve_core_action(action_reference, new_pane_direction, scroll_line_count)
                .map(|command| DispatchPlan::Command(Box::new(command)))
        }
        ActionHandlerReference::CoreClient(client_action_kind) => {
            Ok(DispatchPlan::ClientAction(client_action_kind))
        }
    }
}

/// Build the typed command one built-in action stands for.
///
/// The table is keyed on the action name; a name not in the table is
/// [`ResolveError::ArgumentsRequired`]. Every target field is `None`.
/// `new_pane_direction` fills the split direction of every pane-opening action
/// that does not state one itself.
fn resolve_core_action(
    action_reference: &ActionReference,
    new_pane_direction: Direction,
    scroll_line_count: u16,
) -> Result<Command, ResolveError> {
    Ok(match action_reference.action_name.get_name() {
        // --- Panes ---
        "new-pane" => build_new_pane_command(new_pane_direction),
        "new-pane-left" => build_new_pane_command(Direction::Left),
        "new-pane-down" => build_new_pane_command(Direction::Down),
        "new-pane-up" => build_new_pane_command(Direction::Up),
        "new-pane-right" => build_new_pane_command(Direction::Right),
        "new-pane-stacked" => Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
        "close-pane" => Command::ClosePane(ClosePaneArgs::default()),
        "close-pane-tree" => Command::ClosePane(ClosePaneArgs {
            pane_id: None,
            should_force_close: false,
            should_kill_process_tree: true,
        }),
        "resize-pane-left" => build_resize_pane_command(Direction::Left),
        "resize-pane-down" => build_resize_pane_command(Direction::Down),
        "resize-pane-up" => build_resize_pane_command(Direction::Up),
        "resize-pane-right" => build_resize_pane_command(Direction::Right),
        "focus-pane-left" => build_focus_pane_command(Direction::Left),
        "focus-pane-down" => build_focus_pane_command(Direction::Down),
        "focus-pane-up" => build_focus_pane_command(Direction::Up),
        "focus-pane-right" => build_focus_pane_command(Direction::Right),
        "toggle-pane-fullscreen" => Command::TogglePaneFullscreen,

        // --- Tabs ---
        "new-tab" => Command::NewTab(NewTabArgs::default()),
        "close-tab" => Command::CloseTab(CloseTabArgs::default()),
        "next-tab" => Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
        "previous-tab" => Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Previous,
            client_id: None,
        }),

        // --- Session ---
        "quit" => Command::Quit,

        // --- Lock ---
        "toggle-lock" => Command::ToggleLockMode(ToggleLockModeArgs { client_id: None }),
        "lock" => Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
        "unlock" => Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id: None,
        }),

        // --- Mouse select ---
        "mouse-select" => Command::ToggleMouseSelect,

        // --- Scroll ---
        "scroll-pane-up" => build_scroll_pane_command(scroll_line_count, true),
        "scroll-pane-down" => build_scroll_pane_command(scroll_line_count, false),

        _ => {
            return Err(ResolveError::ArgumentsRequired {
                action_reference: action_reference.clone(),
            })
        }
    })
}

/// The command a `new-pane-<direction>` action builds: split the focused pane
/// and open the new one toward `direction`.
fn build_new_pane_command(direction: Direction) -> Command {
    Command::NewPane(NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    })
}

/// The command a `resize-pane-<direction>` action builds: move the focused
/// pane's border one cell toward `direction`.
fn build_resize_pane_command(direction: Direction) -> Command {
    Command::ResizePane(ResizePaneArgs {
        pane_id: None,
        direction,
        resize_amount_cells: 1,
    })
}

/// The command a `focus-pane-<direction>` action builds: move the issuing
/// client's focus to the neighboring pane toward `direction`.
fn build_focus_pane_command(direction: Direction) -> Command {
    Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Direction(direction),
        client_id: None,
    })
}

/// The command a scroll action builds: `scroll_line_count` lines up when
/// `should_scroll_up`, else down (a negative count).
fn build_scroll_pane_command(scroll_line_count: u16, should_scroll_up: bool) -> Command {
    let scroll_line_count = i32::from(scroll_line_count);
    let signed_scroll_line_count = if should_scroll_up {
        scroll_line_count
    } else {
        scroll_line_count.saturating_neg()
    };
    Command::ScrollPane(ScrollPaneArgs {
        pane_id: None,
        scroll_line_count: signed_scroll_line_count,
    })
}

#[cfg(test)]
mod tests;
