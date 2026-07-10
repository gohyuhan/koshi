//! Action resolution — turning a named action into the plan that runs it.
//!
//! [`action`](crate::action) ships the vocabulary and [`registry`](crate::registry)
//! holds the live table. This module ships the one step between them and the
//! dispatcher: given an [`ActionRef`] such as `core:next-tab` plus the arguments
//! bound to it, produce the [`Command`] the runtime should execute.
//!
//! # Why an action is not just a command
//!
//! An action is the stable user-facing name; a [`Command`] is the internal
//! mutation. The two are not one-to-one. Several actions build the same command
//! and differ only by a value fixed for that action: `lock` and `unlock` both
//! build [`Command::SetLockMode`], `next-tab` and `previous-tab` both build
//! [`Command::FocusTab`]. Those fixed values live in a table keyed on the
//! action's name rather than on its [`CommandKind`](crate::command::CommandKind),
//! which cannot tell those actions apart.
//!
//! # What this module does not decide
//!
//! Every command it builds names its target as `None`, which each argument
//! struct already reads as "the focused one". Choosing the actual pane, tab, or
//! client — and refusing a session with several attached clients and no named
//! target — happens in the runtime's command handlers, which see the issuing
//! [`CommandSource`](crate::command::CommandSource). Resolution is a pure
//! function of the reference, its arguments, and the registry.
//!
//! # Routes
//!
//! A registry entry's [`ActionHandlerRef`] picks one of three plans. A `core:`
//! action builds a typed command. A `plugin:` action becomes a host call, which
//! is the only door a plugin has into the runtime and the place its capability
//! grants are checked. A `user:` macro fans out into the plans of the actions it
//! names, in order, halting on the first failure and bounded by
//! [`MAX_SEQUENCE_DEPTH`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use crate::action::{ActionHandlerRef, ActionRef, ActionStatus};
use crate::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    MoveTabArgs, NewPaneArgs, NewTabArgs, RenamePaneArgs, RenameSessionArgs, RenameTabArgs,
    ResizePaneArgs, RunCommandPaneArgs, TabTarget,
};
use crate::error::{DomainCategory, DomainError, Severity};
use crate::geometry::Direction;
use crate::ids::PluginId;
use crate::process::{ShellKind, SpawnSpec};
use crate::registry::ActionRegistry;
use serde::{Deserialize, Serialize};

/// How many [`ActionHandlerRef::Sequence`] handlers one chain may nest.
///
/// The budget is spent on sequences, not on the actions they name: a chain of
/// eight macros ending in a real action resolves, and a ninth macro inside it
/// does not. A macro that names itself, directly or through a ring of other
/// macros, exhausts the budget instead of recursing forever.
pub const MAX_SEQUENCE_DEPTH: usize = 8;

/// The arguments bound to an action at its call site — a keymap entry, or a step
/// of a macro.
///
/// # What a variant may carry
///
/// Exactly what the invoker chooses, which is three things and no others:
///
/// - **Non-target knobs** — a split direction, a resize amount, a force flag.
/// - **Required targets** — a target the command has no default for, so the
///   invoker must name it. [`FocusPaneArgs::target`](crate::command::FocusPaneArgs)
///   is a [`FocusTarget`], not an `Option`, so [`ActionArgs::FocusPane`]
///   carries one — a pane id, or a direction the runtime resolves against
///   the layout.
/// - **Nothing else.**
///
/// An **optional** target is never here: `None` already reads as "the focused
/// one" in each argument struct, and the runtime resolves it from the command's
/// source. Nor is a field the issuing boundary owns: a pane's `cwd` and `env`
/// are captured where the command is issued, which is why [`ActionArgs::Run`]
/// names a program and its arguments rather than a whole [`SpawnSpec`].
///
/// [`ActionArgs::None`] means no arguments were given. Actions whose every field
/// is optional accept it and fall back to their defaults; actions with a
/// required field reject it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionArgs {
    /// No arguments supplied.
    None,
    /// Arguments for `core:new-pane`.
    NewPane {
        /// Split direction; `None` uses the runtime's default split
        /// direction. Ignored when `stacked`.
        direction: Option<Direction>,
        /// Stack onto the source pane instead of splitting space.
        stacked: bool,
    },
    /// Arguments for `core:close-pane`.
    ClosePane {
        /// Kill the pane's child immediately, overriding its close policy.
        force: bool,
    },
    /// Arguments for `core:resize-pane`.
    ResizePane {
        /// Which of the pane's borders moves.
        direction: Direction,
        /// Signed cell count the border moves; zero is rejected at dispatch.
        size: i16,
    },
    /// Arguments for `core:focus-pane`.
    FocusPane {
        /// The pane to focus, by id or by direction from the focused pane.
        target: FocusTarget,
    },
    /// Arguments for `core:close-tab`.
    CloseTab {
        /// Kill every pane's child immediately, overriding each close policy.
        force: bool,
    },
    /// Arguments for `core:focus-tab`.
    FocusTab {
        /// Which tab to focus.
        target: TabTarget,
    },
    /// Arguments for `core:move-tab`.
    MoveTab {
        /// Destination zero-based index.
        index: usize,
    },
    /// Arguments for `core:run`.
    Run {
        /// The program to execute.
        program: PathBuf,
        /// Arguments passed to the program, excluding `argv[0]`.
        args: Vec<String>,
        /// Split direction for the new pane; `None` uses the runtime's
        /// default split direction.
        direction: Option<Direction>,
        /// Stack onto the source pane instead of splitting space.
        stacked: bool,
    },
}

/// What running one action amounts to.
///
/// The caller submits a [`DispatchPlan::Command`] to the runtime inside a
/// [`CommandEnvelope`](crate::command::CommandEnvelope), forwards a
/// [`DispatchPlan::PluginHostCall`] to the plugin host, and walks a
/// [`DispatchPlan::Sequence`] front to back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchPlan {
    /// Dispatch one typed command.
    Command(Command),
    /// Hand the action to the plugin that owns it.
    PluginHostCall {
        /// The plugin that registered the action.
        plugin: PluginId,
        /// The action it was asked to perform.
        action: ActionRef,
        /// The arguments to forward, uninterpreted.
        args: ActionArgs,
    },
    /// Run each plan in order, stopping at the first that fails.
    Sequence(Vec<DispatchPlan>),
}

/// Why an action could not be turned into a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The reference names no entry in the registry. A binding pointing at it is
    /// an orphan, e.g. because its plugin unloaded.
    Unregistered {
        /// The reference that was not found.
        action: ActionRef,
    },
    /// The action is seeded for completeness but the runtime has no handler for
    /// it yet.
    ComingSoon {
        /// The reference that has no handler.
        action: ActionRef,
    },
    /// The arguments do not fit the action.
    ArgsMismatch {
        /// The reference whose arguments did not fit.
        action: ActionRef,
    },
    /// A macro sits deeper than [`MAX_SEQUENCE_DEPTH`] nested sequences.
    SequenceTooDeep {
        /// The macro resolution gave up on.
        action: ActionRef,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Unregistered { action } => {
                write!(f, "action {action} is not registered")
            }
            ResolveError::ComingSoon { action } => {
                write!(f, "action {action} is not implemented yet")
            }
            ResolveError::ArgsMismatch { action } => {
                write!(f, "action {action} was given arguments it does not accept")
            }
            ResolveError::SequenceTooDeep { action } => write!(
                f,
                "action {action} nests past the maximum of {MAX_SEQUENCE_DEPTH} sequence levels"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl DomainError for ResolveError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Turn an action reference and its arguments into the plan that runs it.
///
/// # Errors
/// - [`ResolveError::Unregistered`] if `action` names no entry in `registry`.
/// - [`ResolveError::ComingSoon`] if the runtime has no handler for it yet.
/// - [`ResolveError::ArgsMismatch`] if `args` do not fit `action`.
/// - [`ResolveError::SequenceTooDeep`] if a macro nests past
///   [`MAX_SEQUENCE_DEPTH`].
pub fn resolve_action(
    action: &ActionRef,
    args: &ActionArgs,
    registry: &ActionRegistry,
) -> Result<DispatchPlan, ResolveError> {
    resolve_at_depth(action, args, registry, 0)
}

/// [`resolve_action`] carrying the count of sequences entered to reach `action`.
fn resolve_at_depth(
    action: &ActionRef,
    args: &ActionArgs,
    registry: &ActionRegistry,
    depth: usize,
) -> Result<DispatchPlan, ResolveError> {
    let metadata = registry
        .lookup(action)
        .ok_or_else(|| ResolveError::Unregistered {
            action: action.clone(),
        })?;

    if metadata.status == ActionStatus::ComingSoon {
        return Err(ResolveError::ComingSoon {
            action: action.clone(),
        });
    }

    match &metadata.handler {
        ActionHandlerRef::CoreCommand(_) => resolve_core(action, args).map(DispatchPlan::Command),
        ActionHandlerRef::PluginHostCall(plugin) => Ok(DispatchPlan::PluginHostCall {
            plugin: *plugin,
            action: action.clone(),
            args: args.clone(),
        }),
        // A sequence names its steps and nothing else, so there is no argument
        // for the macro itself to carry and each step resolves with none. The
        // budget is spent here, where the nesting happens, so a leaf action at
        // the deepest level still resolves.
        ActionHandlerRef::Sequence(steps) => {
            if depth >= MAX_SEQUENCE_DEPTH {
                return Err(ResolveError::SequenceTooDeep {
                    action: action.clone(),
                });
            }
            if args != &ActionArgs::None {
                return Err(ResolveError::ArgsMismatch {
                    action: action.clone(),
                });
            }
            let mut plans = Vec::with_capacity(steps.len());
            for step in steps {
                plans.push(resolve_at_depth(
                    step,
                    &ActionArgs::None,
                    registry,
                    depth + 1,
                )?);
            }
            Ok(DispatchPlan::Sequence(plans))
        }
    }
}

/// Build the typed command one built-in action stands for.
///
/// The name and the arguments are matched together, so an argument shape that
/// belongs to a different action falls through to
/// [`ResolveError::ArgsMismatch`]. Targets are left `None` for the runtime to
/// resolve against the command's source.
///
/// The action's [`CommandKind`](crate::command::CommandKind) is deliberately not
/// consulted: nine actions share `CopyMode` and two share `SetLockMode`, so the
/// discriminant cannot say which command to build. It stays on the metadata as
/// the introspection surface, and a test pins it against what this table
/// produces.
fn resolve_core(action: &ActionRef, args: &ActionArgs) -> Result<Command, ResolveError> {
    let command = match (action.name.as_str(), args) {
        // --- Panes ---
        ("new-pane", ActionArgs::None) => Command::NewPane(NewPaneArgs::default()),
        ("new-pane", ActionArgs::NewPane { direction, stacked }) => Command::NewPane(NewPaneArgs {
            source: None,
            direction: *direction,
            stacked: *stacked,
            cwd: None,
            command: None,
            client: None,
        }),
        ("close-pane", ActionArgs::None) => Command::ClosePane(ClosePaneArgs::default()),
        ("close-pane", ActionArgs::ClosePane { force }) => Command::ClosePane(ClosePaneArgs {
            pane: None,
            force: *force,
        }),
        ("resize-pane", ActionArgs::ResizePane { direction, size }) => {
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: *direction,
                size: *size,
            })
        }
        ("focus-pane", ActionArgs::FocusPane { target }) => Command::FocusPane(FocusPaneArgs {
            target: *target,
            client: None,
        }),
        ("toggle-pane-fullscreen", ActionArgs::None) => Command::TogglePaneFullscreen,
        ("rename-pane", ActionArgs::None) => Command::RenamePane(RenamePaneArgs { pane: None }),

        // --- Tabs ---
        ("new-tab", ActionArgs::None) => Command::NewTab(NewTabArgs::default()),
        ("close-tab", ActionArgs::None) => Command::CloseTab(CloseTabArgs::default()),
        ("close-tab", ActionArgs::CloseTab { force }) => Command::CloseTab(CloseTabArgs {
            tab: None,
            force: *force,
        }),
        ("focus-tab", ActionArgs::FocusTab { target }) => Command::FocusTab(FocusTabArgs {
            target: *target,
            client: None,
        }),
        ("next-tab", ActionArgs::None) => Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
        }),
        ("previous-tab", ActionArgs::None) => Command::FocusTab(FocusTabArgs {
            target: TabTarget::Prev,
            client: None,
        }),
        ("rename-tab", ActionArgs::None) => Command::RenameTab(RenameTabArgs { tab: None }),
        ("move-tab", ActionArgs::MoveTab { index }) => Command::MoveTab(MoveTabArgs {
            tab: None,
            index: *index,
        }),

        // --- Session ---
        ("rename-session", ActionArgs::None) => {
            Command::RenameSession(RenameSessionArgs { session: None })
        }

        // --- Lock ---
        ("toggle-lock", ActionArgs::None) => Command::ToggleLockMode,
        ("lock", ActionArgs::None) => Command::SetLockMode(LockModeArgs { locked: true }),
        ("unlock", ActionArgs::None) => Command::SetLockMode(LockModeArgs { locked: false }),

        // --- Run ---
        // The spawn spec is built here rather than accepted from the caller: a
        // pane's `cwd` and `env` belong to the boundary that issues the command,
        // and a spec supplied whole would carry both.
        (
            "run",
            ActionArgs::Run {
                program,
                args,
                direction,
                stacked,
            },
        ) => Command::RunCommandPane(RunCommandPaneArgs {
            command: SpawnSpec {
                program: program.clone(),
                args: args.clone(),
                cwd: None,
                env: BTreeMap::new(),
                shell_kind: ShellKind::from_program(program),
            },
            cwd: None,
            source: None,
            direction: *direction,
            stacked: *stacked,
        }),

        _ => {
            return Err(ResolveError::ArgsMismatch {
                action: action.clone(),
            })
        }
    };
    Ok(command)
}

#[cfg(test)]
mod tests;
