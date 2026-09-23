//! Action resolution — turning a named action into the plan that runs it.
//!
//! [`action`](crate::action) ships the vocabulary and [`registry`](crate::registry)
//! holds the live table. This module ships the one step between them and the
//! dispatcher: given an [`ActionReference`] such as `core:next-tab` plus the arguments
//! bound to it, produce the [`Command`] the runtime should execute.
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
//! reference, its arguments, the registry, and the caller's own split
//! direction: the caller passes in its `layout.new-pane-direction`, and a
//! pane-opening action that names no side of its own is built with it.
//!
//! # Routes
//!
//! A registry entry's [`ActionHandlerReference`] picks one of four plans. A
//! [`CoreCommand`](ActionHandlerReference::CoreCommand) builds a typed command. A
//! [`CoreClient`](ActionHandlerReference::CoreClient) returns a viewer-local action.
//! [`PluginHostCall`](ActionHandlerReference::PluginHostCall) becomes a
//! [`DispatchPlan::PluginHostCall`] carrying the arguments uninterpreted. A
//! [`Sequence`](ActionHandlerReference::Sequence) fans out into the plans of the
//! actions it names, in order, halting on the first failure and bounded by
//! [`MAX_SEQUENCE_DEPTH`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use crate::action::{ActionHandlerReference, ActionReference, ActionStatus, ClientActionKind};
use crate::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MovePaneArgs, NewPaneArgs, NewTabArgs, ResizePaneArgs, RunCommandPaneArgs, ScrollPaneArgs,
    TabTarget, ToggleLockModeArgs,
};
use crate::error::{DomainCategory, DomainError, Severity};
use crate::geometry::Direction;
use crate::ids::PluginId;
use crate::process::{ShellKind, SpawnSpec};
use crate::registry::ActionRegistry;
use serde::{Deserialize, Serialize};

/// How many [`ActionHandlerReference::Sequence`] handlers one chain may nest.
///
/// The budget is spent on sequences, not on the actions they name: a chain of
/// eight macros ending in a real action resolves, and a ninth macro inside it
/// does not. A macro that names itself, directly or through a cycle of other
/// macros, exhausts the budget.
pub const MAX_SEQUENCE_DEPTH: usize = 8;

/// The number of lines a scroll action uses when its binding gives no value.
pub const DEFAULT_SCROLL_LINE_COUNT: u16 = 3;

/// The arguments bound to an action at its call site — a keymap entry, or a step
/// of a macro.
///
/// A choice with a small fixed set of values lives in the action NAME
/// (`core:new-pane-left`, `core:close-pane-tree`). A variant here is a
/// SYSTEM-authored preset: a user-authored keymap layer has every binding's
/// arguments replaced with [`ActionArgs::None`] on load.
///
/// [`ActionArgs::Run`] names a program and its arguments, not a whole
/// [`SpawnSpec`]: the command it builds carries no working directory and an
/// empty environment. [`ActionArgs::Scroll`] supplies an optional signed line
/// count to the scroll actions.
///
/// Every binding-invocable `core:` action except `core:run` and the scroll
/// actions accepts only [`ActionArgs::None`]; `core:run` accepts only
/// [`ActionArgs::Run`]. CLI-only actions are built directly by the CLI. A
/// plugin action forwards any value uninterpreted. A macro accepts only
/// [`ActionArgs::None`], and every step of it resolves with
/// [`ActionArgs::None`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionArgs {
    /// No arguments supplied.
    None,
    /// Arguments for `core:run`.
    Run {
        /// The program to execute.
        program: PathBuf,
        /// Arguments passed to the program, excluding `argv[0]`.
        arguments: Vec<String>,
        /// Split direction for the new pane; `None` uses the direction the
        /// resolving client passes in.
        direction: Option<Direction>,
        /// Stack onto the source pane instead of splitting space.
        should_stack: bool,
    },
    /// Optional signed scroll line count for `core:scroll-pane-up` and
    /// `core:scroll-pane-down`; `None` uses the viewer's configured default.
    Scroll {
        /// Signed scroll line count to place in the typed scroll command.
        scroll_line_count: Option<i32>,
    },
}

/// What running one action amounts to: one command, one plugin call, or a
/// list of plans in the order they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchPlan {
    /// Dispatch one typed command.
    Command(Command),
    /// Run one viewer-local action without sending a session command.
    ClientAction(ClientActionKind),
    /// Hand the action to the plugin that owns it.
    PluginHostCall {
        /// The plugin that registered the action.
        plugin_id: PluginId,
        /// The action it was asked to perform.
        action_reference: ActionReference,
        /// The arguments to forward, uninterpreted.
        action_arguments: ActionArgs,
    },
    /// Run each plan in order, stopping at the first that fails.
    Sequence(Vec<DispatchPlan>),
}

/// Why an action could not be turned into a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The reference names no registry record.
    Unregistered {
        /// The reference that was not found.
        action_reference: ActionReference,
    },
    /// The registry record's status is [`ActionStatus::ComingSoon`].
    ComingSoon {
        /// The reference whose status is `ComingSoon`.
        action_reference: ActionReference,
    },
    /// The arguments do not fit the action.
    ArgsMismatch {
        /// The reference whose arguments did not fit.
        action_reference: ActionReference,
    },
    /// A macro sits deeper than [`MAX_SEQUENCE_DEPTH`] nested sequences.
    SequenceTooDeep {
        /// The macro resolution gave up on.
        action_reference: ActionReference,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Unregistered { action_reference } => {
                write!(f, "action {action_reference} is not registered")
            }
            ResolveError::ComingSoon { action_reference } => {
                write!(f, "action {action_reference} is not implemented yet")
            }
            ResolveError::ArgsMismatch { action_reference } => {
                write!(f, "action {action_reference} was given arguments it does not accept")
            }
            ResolveError::SequenceTooDeep { action_reference } => write!(
                f,
                "action {action_reference} nests past the maximum of {MAX_SEQUENCE_DEPTH} sequence levels"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl DomainError for ResolveError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Turn an action reference and its arguments into the plan that runs it.
///
/// `new_pane_direction` is the caller's own `layout.new-pane-direction`
/// setting. Actions that open a pane without naming a direction —
/// `core:new-pane`, `core:new-pane-stacked`, and `core:run` with
/// `direction: None` — build their command with it.
///
/// # Errors
/// - [`ResolveError::Unregistered`] if `action_reference` names no registry record in `registry`.
/// - [`ResolveError::ComingSoon`] if the registry record's status is
///   [`ActionStatus::ComingSoon`].
/// - [`ResolveError::ArgsMismatch`] if `action_arguments` do not fit `action_reference`.
/// - [`ResolveError::SequenceTooDeep`] if a macro nests past
///   [`MAX_SEQUENCE_DEPTH`].
pub fn resolve_action(
    action_reference: &ActionReference,
    action_arguments: &ActionArgs,
    registry: &ActionRegistry,
    new_pane_direction: Direction,
) -> Result<DispatchPlan, ResolveError> {
    resolve_action_with_scroll_line_count(
        action_reference,
        action_arguments,
        registry,
        new_pane_direction,
        DEFAULT_SCROLL_LINE_COUNT,
    )
}

/// Turn an action into a plan with the caller's configured default scroll count.
///
/// The regular [`resolve_action`] entry point uses [`DEFAULT_SCROLL_LINE_COUNT`].
/// A client uses this entry point when its mouse configuration supplies another
/// count for a scroll action that has no explicit scroll line count.
pub fn resolve_action_with_scroll_line_count(
    action_reference: &ActionReference,
    action_arguments: &ActionArgs,
    registry: &ActionRegistry,
    new_pane_direction: Direction,
    scroll_line_count: u16,
) -> Result<DispatchPlan, ResolveError> {
    resolve_action_at_depth(
        action_reference,
        action_arguments,
        registry,
        new_pane_direction,
        scroll_line_count,
        0,
    )
}

/// [`resolve_action_with_scroll_line_count`] carrying the count of sequences entered to reach `action`.
fn resolve_action_at_depth(
    action_reference: &ActionReference,
    action_arguments: &ActionArgs,
    registry: &ActionRegistry,
    new_pane_direction: Direction,
    scroll_line_count: u16,
    sequence_depth: usize,
) -> Result<DispatchPlan, ResolveError> {
    let action_metadata = registry
        .find_action_metadata(action_reference)
        .ok_or_else(|| ResolveError::Unregistered {
            action_reference: action_reference.clone(),
        })?;

    if action_metadata.action_status == ActionStatus::ComingSoon {
        return Err(ResolveError::ComingSoon {
            action_reference: action_reference.clone(),
        });
    }

    match &action_metadata.handler {
        ActionHandlerReference::CoreCommand(_) => resolve_core_action(
            action_reference,
            action_arguments,
            new_pane_direction,
            scroll_line_count,
        )
        .map(DispatchPlan::Command),
        ActionHandlerReference::CoreClient(client_action_kind) => {
            if action_arguments != &ActionArgs::None {
                return Err(ResolveError::ArgsMismatch {
                    action_reference: action_reference.clone(),
                });
            }
            Ok(DispatchPlan::ClientAction(*client_action_kind))
        }
        ActionHandlerReference::PluginHostCall(plugin_id) => Ok(DispatchPlan::PluginHostCall {
            plugin_id: *plugin_id,
            action_reference: action_reference.clone(),
            action_arguments: action_arguments.clone(),
        }),
        // Every step resolves with `ActionArgs::None`. `sequence_depth` counts the
        // sequences entered, not the actions reached: a leaf action below the
        // deepest allowed sequence resolves.
        ActionHandlerReference::Sequence(action_steps) => {
            if sequence_depth >= MAX_SEQUENCE_DEPTH {
                return Err(ResolveError::SequenceTooDeep {
                    action_reference: action_reference.clone(),
                });
            }
            if action_arguments != &ActionArgs::None {
                return Err(ResolveError::ArgsMismatch {
                    action_reference: action_reference.clone(),
                });
            }
            let mut dispatch_plans = Vec::with_capacity(action_steps.len());
            for action_step in action_steps.iter() {
                dispatch_plans.push(resolve_action_at_depth(
                    action_step,
                    &ActionArgs::None,
                    registry,
                    new_pane_direction,
                    scroll_line_count,
                    sequence_depth + 1,
                )?);
            }
            Ok(DispatchPlan::Sequence(dispatch_plans))
        }
    }
}

/// Build the typed command one built-in action stands for.
///
/// The name and the arguments are matched together; a pair not in the table
/// is [`ResolveError::ArgsMismatch`]. Every target field is `None`.
/// `new_pane_direction` fills the split direction of every pane-opening action
/// that does not state one itself.
fn resolve_core_action(
    action_reference: &ActionReference,
    action_arguments: &ActionArgs,
    new_pane_direction: Direction,
    scroll_line_count: u16,
) -> Result<Command, ResolveError> {
    Ok(
        match (action_reference.action_name.get_name(), action_arguments) {
            // --- Panes ---
            ("new-pane", ActionArgs::None) => build_new_pane_command(new_pane_direction),
            ("new-pane-left", ActionArgs::None) => build_new_pane_command(Direction::Left),
            ("new-pane-down", ActionArgs::None) => build_new_pane_command(Direction::Down),
            ("new-pane-up", ActionArgs::None) => build_new_pane_command(Direction::Up),
            ("new-pane-right", ActionArgs::None) => build_new_pane_command(Direction::Right),
            ("new-pane-stacked", ActionArgs::None) => Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: new_pane_direction,
                should_stack: true,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
            ("close-pane", ActionArgs::None) => Command::ClosePane(ClosePaneArgs::default()),
            ("close-pane-tree", ActionArgs::None) => Command::ClosePane(ClosePaneArgs {
                pane_id: None,
                should_force_close: false,
                should_kill_process_tree: true,
            }),
            ("resize-pane-left", ActionArgs::None) => build_resize_pane_command(Direction::Left),
            ("resize-pane-down", ActionArgs::None) => build_resize_pane_command(Direction::Down),
            ("resize-pane-up", ActionArgs::None) => build_resize_pane_command(Direction::Up),
            ("resize-pane-right", ActionArgs::None) => build_resize_pane_command(Direction::Right),
            ("focus-pane-left", ActionArgs::None) => build_focus_pane_command(Direction::Left),
            ("focus-pane-down", ActionArgs::None) => build_focus_pane_command(Direction::Down),
            ("focus-pane-up", ActionArgs::None) => build_focus_pane_command(Direction::Up),
            ("focus-pane-right", ActionArgs::None) => build_focus_pane_command(Direction::Right),
            ("move-pane-left", ActionArgs::None) => build_move_pane_command(Direction::Left),
            ("move-pane-down", ActionArgs::None) => build_move_pane_command(Direction::Down),
            ("move-pane-up", ActionArgs::None) => build_move_pane_command(Direction::Up),
            ("move-pane-right", ActionArgs::None) => build_move_pane_command(Direction::Right),
            ("toggle-pane-fullscreen", ActionArgs::None) => Command::TogglePaneFullscreen,

            // --- Tabs ---
            ("new-tab", ActionArgs::None) => Command::NewTab(NewTabArgs::default()),
            ("close-tab", ActionArgs::None) => Command::CloseTab(CloseTabArgs::default()),
            ("next-tab", ActionArgs::None) => Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            }),
            ("previous-tab", ActionArgs::None) => Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Prev,
                client_id: None,
            }),

            // --- Session ---
            ("quit", ActionArgs::None) => Command::Quit,

            // --- Lock ---
            ("toggle-lock", ActionArgs::None) => {
                Command::ToggleLockMode(ToggleLockModeArgs { client_id: None })
            }
            ("lock", ActionArgs::None) => Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
            ("unlock", ActionArgs::None) => Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
            }),

            // --- Mouse select ---
            ("mouse-select", ActionArgs::None) => Command::ToggleMouseSelect,

            // --- Scroll ---
            ("scroll-pane-up", ActionArgs::None) => {
                build_scroll_pane_command(None, scroll_line_count, true)
            }
            (
                "scroll-pane-up",
                ActionArgs::Scroll {
                    scroll_line_count: requested_scroll_line_count,
                },
            ) => build_scroll_pane_command(*requested_scroll_line_count, scroll_line_count, true),
            ("scroll-pane-down", ActionArgs::None) => {
                build_scroll_pane_command(None, scroll_line_count, false)
            }
            (
                "scroll-pane-down",
                ActionArgs::Scroll {
                    scroll_line_count: requested_scroll_line_count,
                },
            ) => build_scroll_pane_command(*requested_scroll_line_count, scroll_line_count, false),

            // --- Run ---
            // The spawn spec carries no working directory and an empty
            // environment.
            (
                "run",
                ActionArgs::Run {
                    program,
                    arguments,
                    direction,
                    should_stack,
                },
            ) => Command::RunCommandPane(RunCommandPaneArgs {
                spawn_spec: SpawnSpec {
                    program: program.clone(),
                    arguments: arguments.clone(),
                    working_directory: None,
                    environment_variables: BTreeMap::new(),
                    shell_kind: ShellKind::from_program(program),
                },
                working_directory: None,
                source_pane_id: None,
                tab_id: None,
                direction: direction.unwrap_or(new_pane_direction),
                should_stack: *should_stack,
                client_id: None,
            }),

            _ => {
                return Err(ResolveError::ArgsMismatch {
                    action_reference: action_reference.clone(),
                })
            }
        },
    )
}

/// The command a `new-pane-<direction>` action builds: split the focused pane
/// and open the new one toward `direction`.
fn build_new_pane_command(direction: Direction) -> Command {
    Command::NewPane(NewPaneArgs {
        source_pane_id: None,
        tab_id: None,
        direction,
        should_stack: false,
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

/// The command a `move-pane-<direction>` action builds.
fn build_move_pane_command(direction: Direction) -> Command {
    Command::MovePane(MovePaneArgs {
        pane_id: None,
        direction,
    })
}

/// The command a scroll action builds from its optional line count.
fn build_scroll_pane_command(
    requested_scroll_line_count: Option<i32>,
    default_scroll_line_count: u16,
    should_scroll_up: bool,
) -> Command {
    let default_scroll_line_count = i32::from(default_scroll_line_count);
    let scroll_line_count = requested_scroll_line_count.unwrap_or(default_scroll_line_count);
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
