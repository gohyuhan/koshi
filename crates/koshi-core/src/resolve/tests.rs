//! Unit tests for action resolution: the built-in name table, the named
//! variants that distinguish actions sharing one command, argument fit, the
//! plugin and macro routes, and the orphan and coming-soon refusals.

use super::*;

use crate::action::{core_action_seeds, ActionMetadata, ActionNamespace, ActionScope, TargetKind};
use crate::command::CommandKind;
use crate::process::ShellKind;
use crate::registry::tests::insert_unchecked;
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

/// A `core:` reference for a name known to satisfy the grammar.
fn core(name: &str) -> ActionRef {
    ActionRef::core(name).expect("test action name is valid")
}

/// A plugin id built from a fixed uuid, so the same byte yields the same plugin.
fn plugin_id(byte: u8) -> PluginId {
    PluginId::from_uuid(Uuid::from_bytes([byte; 16]))
}

/// The program `core:run` is exercised with.
fn run_program() -> PathBuf {
    PathBuf::from("/usr/bin/lazygit")
}

/// The spawn spec `core:run` must build from [`run_program`]: no `cwd`, no
/// `env`, and a shell kind derived from the program.
fn spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: run_program(),
        args: vec!["--all".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("lazygit".to_string()),
    }
}

/// The `Available` core actions that no binding can invoke: each has a
/// required value with an open range (a resize amount, a pane id, a tab
/// index, the text to type), so it is reachable only through a CLI command,
/// which builds its [`Command`] directly. `resolve_action` refuses every one
/// of them whatever the arguments. Pinned against the seed table by
/// [`available_table_matches_seeds`].
const CLI_ONLY: [&str; 5] = [
    "resize-pane",
    "focus-pane",
    "focus-tab",
    "move-tab",
    "write-to-pane",
];

/// Every binding-invocable `Available` core action, the arguments it is
/// invoked with, and the exact command it must produce. Together with
/// [`CLI_ONLY`] this covers the whole `Available` seed set, pinned by
/// [`available_table_matches_seeds`], so an action that gains or loses
/// `Available` status without a matching row fails the suite.
fn available_table() -> Vec<(&'static str, ActionArgs, Command)> {
    vec![
        (
            "new-pane",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs::default()),
        ),
        (
            "new-pane-left",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(Direction::Left),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            "new-pane-down",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(Direction::Down),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            "new-pane-up",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(Direction::Up),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            "new-pane-right",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(Direction::Right),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            "new-pane-stacked",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: None,
                stacked: true,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            "close-pane",
            ActionArgs::None,
            Command::ClosePane(ClosePaneArgs::default()),
        ),
        (
            "close-pane-tree",
            ActionArgs::None,
            Command::ClosePane(ClosePaneArgs {
                pane: None,
                force: false,
                tree: true,
            }),
        ),
        (
            "resize-pane-left",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Left,
                size: 1,
            }),
        ),
        (
            "resize-pane-down",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Down,
                size: 1,
            }),
        ),
        (
            "resize-pane-up",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Up,
                size: 1,
            }),
        ),
        (
            "resize-pane-right",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Right,
                size: 1,
            }),
        ),
        (
            "focus-pane-left",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Direction(Direction::Left),
                client: None,
            }),
        ),
        (
            "focus-pane-down",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Direction(Direction::Down),
                client: None,
            }),
        ),
        (
            "focus-pane-up",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Direction(Direction::Up),
                client: None,
            }),
        ),
        (
            "focus-pane-right",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Direction(Direction::Right),
                client: None,
            }),
        ),
        (
            "toggle-pane-fullscreen",
            ActionArgs::None,
            Command::TogglePaneFullscreen,
        ),
        (
            "rename-pane",
            ActionArgs::None,
            Command::RenamePane(RenamePaneArgs { pane: None }),
        ),
        (
            "new-tab",
            ActionArgs::None,
            Command::NewTab(NewTabArgs::default()),
        ),
        (
            "close-tab",
            ActionArgs::None,
            Command::CloseTab(CloseTabArgs::default()),
        ),
        (
            "next-tab",
            ActionArgs::None,
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: None,
            }),
        ),
        (
            "previous-tab",
            ActionArgs::None,
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Prev,
                client: None,
            }),
        ),
        (
            "rename-tab",
            ActionArgs::None,
            Command::RenameTab(RenameTabArgs { tab: None }),
        ),
        (
            "rename-session",
            ActionArgs::None,
            Command::RenameSession(RenameSessionArgs { session: None }),
        ),
        ("quit", ActionArgs::None, Command::Quit),
        ("toggle-lock", ActionArgs::None, Command::ToggleLockMode),
        (
            "lock",
            ActionArgs::None,
            Command::SetLockMode(LockModeArgs { locked: true }),
        ),
        (
            "unlock",
            ActionArgs::None,
            Command::SetLockMode(LockModeArgs { locked: false }),
        ),
        (
            "run",
            ActionArgs::Run {
                program: run_program(),
                args: vec!["--all".to_string()],
                direction: Some(Direction::Down),
                stacked: false,
            },
            Command::RunCommandPane(RunCommandPaneArgs {
                command: spawn_spec(),
                cwd: None,
                source: None,
                direction: Some(Direction::Down),
                stacked: false,
            }),
        ),
    ]
}

/// Metadata a plugin's own registration carries: its namespace, and a handler
/// routing back to itself.
fn plugin_metadata(owner: PluginId) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::Plugin(owner),
        display_name: "Open Status".to_string(),
        description: "Open the status view".to_string(),
        scope_class: ActionScope::Global,
        target_compat: vec![TargetKind::Session],
        args_schema: None,
        handler: ActionHandlerRef::PluginHostCall(owner),
        status: ActionStatus::Available,
        continuous: false,
    }
}

/// Metadata for a `user:` macro whose handler fires `steps` in order.
fn macro_metadata(steps: Vec<ActionRef>) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::User,
        display_name: "Macro".to_string(),
        description: "A user macro".to_string(),
        scope_class: ActionScope::Global,
        target_compat: vec![TargetKind::Session],
        args_schema: None,
        handler: ActionHandlerRef::Sequence(steps),
        status: ActionStatus::Available,
        continuous: false,
    }
}

/// A `user:` reference for a name known to satisfy the grammar.
fn user(name: &str) -> ActionRef {
    ActionRef::user(name).expect("test macro name is valid")
}

/// A registry holding the core seeds plus one `user:` macro whose handler is the
/// given sequence. `register` refuses `user:` references, so the entry goes in
/// through [`insert_unchecked`].
fn registry_with_macro(name: &str, steps: Vec<ActionRef>) -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    insert_unchecked(&mut registry, user(name), macro_metadata(steps));
    registry
}

/// A registry holding a chain of `levels` nested macros — `m0` names `m1`, which
/// names `m2`, and so on — the innermost naming `core:lock`. Returns the
/// registry and the outermost reference.
///
/// The chain is what distinguishes counting sequences from counting
/// resolutions: it has `levels` sequence handlers and one leaf action beneath
/// them.
fn registry_with_macro_chain(levels: usize) -> (ActionRegistry, ActionRef) {
    let mut registry = ActionRegistry::new();
    for level in 0..levels {
        let step = if level + 1 == levels {
            core("lock")
        } else {
            user(&format!("m{}", level + 1))
        };
        insert_unchecked(
            &mut registry,
            user(&format!("m{level}")),
            macro_metadata(vec![step]),
        );
    }
    (registry, user("m0"))
}

#[test]
fn available_table_matches_seeds() {
    let mut seeded: Vec<String> = core_action_seeds()
        .into_iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::Available)
        .map(|(action, _)| action.name.as_str().to_string())
        .collect();
    seeded.sort();

    let mut tabled: Vec<String> = available_table()
        .into_iter()
        .map(|(name, _, _)| name.to_string())
        .chain(CLI_ONLY.into_iter().map(str::to_string))
        .collect();
    tabled.sort();

    assert_eq!(seeded, tabled);
}

#[test]
fn every_available_action_resolves_to_its_exact_command() {
    let registry = ActionRegistry::new();
    for (name, args, expected) in available_table() {
        let plan = resolve_action(&core(name), &args, &registry)
            .unwrap_or_else(|err| panic!("core:{name} must resolve, got {err}"));
        assert_eq!(plan, DispatchPlan::Command(expected), "core:{name}");
    }
}

#[test]
fn resolved_command_kind_matches_the_seeded_handler() {
    let registry = ActionRegistry::new();
    for (name, args, _) in available_table() {
        let action = core(name);
        let metadata = registry.lookup(&action).expect("seed is registered");
        let ActionHandlerRef::CoreCommand(kind) = metadata.handler else {
            panic!("core:{name} must dispatch a core command");
        };
        let Ok(DispatchPlan::Command(command)) = resolve_action(&action, &args, &registry) else {
            panic!("core:{name} must resolve to a command");
        };
        assert_eq!(command.kind(), kind, "core:{name}");
    }
}

#[test]
fn coming_soon_actions_are_refused() {
    let registry = ActionRegistry::new();
    let coming_soon: Vec<ActionRef> = core_action_seeds()
        .into_iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::ComingSoon)
        .map(|(action, _)| action)
        .collect();

    assert_eq!(coming_soon.len(), 15);
    for action in coming_soon {
        assert_eq!(
            resolve_action(&action, &ActionArgs::None, &registry),
            Err(ResolveError::ComingSoon {
                action: action.clone()
            }),
            "{action}"
        );
    }
}

#[test]
fn coming_soon_names_are_pinned() {
    let mut names: Vec<String> = core_action_seeds()
        .into_iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::ComingSoon)
        .map(|(action, _)| action.name.as_str().to_string())
        .collect();
    names.sort();

    assert_eq!(
        names,
        vec![
            "copy-mode-clear-selection",
            "copy-mode-copy",
            "copy-mode-enter",
            "copy-mode-exit",
            "copy-mode-move-cursor",
            "copy-mode-search",
            "copy-mode-search-next",
            "copy-mode-search-prev",
            "copy-mode-set-selection",
            "plugin-disable",
            "plugin-enable",
            "plugin-install",
            "plugin-reload",
            "plugin-uninstall",
            "plugin-update",
        ]
    );
}

#[test]
fn unregistered_action_is_an_orphan() {
    let registry = ActionRegistry::new();
    let action = ActionRef::plugin(plugin_id(1), "open-status").expect("valid name");

    assert_eq!(
        resolve_action(&action, &ActionArgs::None, &registry),
        Err(ResolveError::Unregistered {
            action: action.clone()
        })
    );
}

#[test]
fn plugin_action_routes_to_its_own_host_call() {
    let owner = plugin_id(1);
    let action = ActionRef::plugin(owner, "open-status").expect("valid name");
    let mut registry = ActionRegistry::new();
    registry
        .register(owner, action.clone(), plugin_metadata(owner))
        .expect("plugin registers its own action");

    let args = ActionArgs::Run {
        program: run_program(),
        args: vec![],
        direction: None,
        stacked: false,
    };
    assert_eq!(
        resolve_action(&action, &args, &registry),
        Ok(DispatchPlan::PluginHostCall {
            plugin: owner,
            action,
            args,
        })
    );
}

#[test]
fn cli_only_actions_are_refused_with_and_without_arguments() {
    // `run` joins the CLI-only set for the argless half: a bare `core:run`
    // names no program, so it rejects `None` like the others; unlike them it
    // does accept its own `Run` arguments (covered by the available table).
    let registry = ActionRegistry::new();
    let some_args = ActionArgs::Run {
        program: run_program(),
        args: vec![],
        direction: None,
        stacked: false,
    };
    for name in CLI_ONLY {
        let action = core(name);
        assert_eq!(
            resolve_action(&action, &ActionArgs::None, &registry),
            Err(ResolveError::ArgsMismatch {
                action: action.clone()
            }),
            "core:{name} given no arguments"
        );
        assert_eq!(
            resolve_action(&action, &some_args, &registry),
            Err(ResolveError::ArgsMismatch {
                action: action.clone()
            }),
            "core:{name} given arguments"
        );
    }
    let run = core("run");
    assert_eq!(
        resolve_action(&run, &ActionArgs::None, &registry),
        Err(ResolveError::ArgsMismatch { action: run })
    );
}

#[test]
fn arguments_belonging_to_another_action_are_refused() {
    let registry = ActionRegistry::new();
    let action = core("next-tab");

    assert_eq!(
        resolve_action(
            &action,
            &ActionArgs::Run {
                program: run_program(),
                args: vec![],
                direction: None,
                stacked: false,
            },
            &registry
        ),
        Err(ResolveError::ArgsMismatch {
            action: action.clone()
        })
    );
}

#[test]
fn a_sequence_resolves_each_step_in_order() {
    let registry = registry_with_macro("split-and-lock", vec![core("new-pane"), core("lock")]);
    let macro_ref = user("split-and-lock");

    assert_eq!(
        resolve_action(&macro_ref, &ActionArgs::None, &registry),
        Ok(DispatchPlan::Sequence(vec![
            DispatchPlan::Command(Command::NewPane(NewPaneArgs::default())),
            DispatchPlan::Command(Command::SetLockMode(LockModeArgs { locked: true })),
        ]))
    );
}

#[test]
fn a_sequence_halts_on_the_first_failing_step() {
    let registry = registry_with_macro(
        "lock-then-copy",
        vec![core("lock"), core("copy-mode-enter"), core("unlock")],
    );
    let macro_ref = user("lock-then-copy");

    assert_eq!(
        resolve_action(&macro_ref, &ActionArgs::None, &registry),
        Err(ResolveError::ComingSoon {
            action: core("copy-mode-enter"),
        })
    );
}

#[test]
fn a_sequence_given_arguments_is_refused() {
    let registry = registry_with_macro("split-and-lock", vec![core("new-pane")]);
    let macro_ref = user("split-and-lock");

    assert_eq!(
        resolve_action(
            &macro_ref,
            &ActionArgs::Run {
                program: run_program(),
                args: vec![],
                direction: None,
                stacked: false,
            },
            &registry
        ),
        Err(ResolveError::ArgsMismatch {
            action: macro_ref.clone()
        })
    );
}

#[test]
fn a_self_referencing_macro_exhausts_the_depth_budget() {
    let macro_ref = user("loop");
    let registry = registry_with_macro("loop", vec![macro_ref.clone()]);

    assert_eq!(
        resolve_action(&macro_ref, &ActionArgs::None, &registry),
        Err(ResolveError::SequenceTooDeep {
            action: macro_ref.clone()
        })
    );
}

#[test]
fn a_chain_of_exactly_max_depth_sequences_resolves() {
    let (registry, outermost) = registry_with_macro_chain(MAX_SEQUENCE_DEPTH);

    // The leaf action sits one level below the deepest sequence, and resolves:
    // the budget counts the sequences entered, not the actions reached.
    let mut plan = resolve_action(&outermost, &ActionArgs::None, &registry)
        .expect("a chain at the documented limit must resolve");
    for _ in 0..MAX_SEQUENCE_DEPTH - 1 {
        let DispatchPlan::Sequence(mut steps) = plan else {
            panic!("every level but the last is a sequence");
        };
        assert_eq!(steps.len(), 1);
        plan = steps.remove(0);
    }

    assert_eq!(
        plan,
        DispatchPlan::Sequence(vec![DispatchPlan::Command(Command::SetLockMode(
            LockModeArgs { locked: true }
        ))])
    );
}

#[test]
fn a_chain_one_sequence_past_max_depth_is_refused() {
    let (registry, outermost) = registry_with_macro_chain(MAX_SEQUENCE_DEPTH + 1);

    assert_eq!(
        resolve_action(&outermost, &ActionArgs::None, &registry),
        Err(ResolveError::SequenceTooDeep {
            // The macro at the deepest allowed level is the one refused.
            action: user(&format!("m{MAX_SEQUENCE_DEPTH}")),
        })
    );
}

#[test]
fn run_never_carries_a_cwd_or_env_from_its_caller() {
    let registry = ActionRegistry::new();
    let args = ActionArgs::Run {
        program: run_program(),
        args: vec!["--all".to_string()],
        direction: None,
        stacked: false,
    };

    let Ok(DispatchPlan::Command(Command::RunCommandPane(built))) =
        resolve_action(&core("run"), &args, &registry)
    else {
        panic!("core:run must resolve to a run-command-pane command");
    };

    assert_eq!(built.cwd, None);
    assert_eq!(built.command.cwd, None);
    assert_eq!(built.command.env, BTreeMap::new());
    assert_eq!(built.command.program, run_program());
    assert_eq!(built.command.args, vec!["--all".to_string()]);
    assert_eq!(
        built.command.shell_kind,
        ShellKind::Other("lazygit".to_string())
    );
}

#[test]
fn resolve_error_messages_name_the_action() {
    let action = core("new-pane");

    assert_eq!(
        ResolveError::Unregistered {
            action: action.clone()
        }
        .to_string(),
        "action core:new-pane is not registered"
    );
    assert_eq!(
        ResolveError::ComingSoon {
            action: action.clone()
        }
        .to_string(),
        "action core:new-pane is not implemented yet"
    );
    assert_eq!(
        ResolveError::ArgsMismatch {
            action: action.clone()
        }
        .to_string(),
        "action core:new-pane was given arguments it does not accept"
    );
    assert_eq!(
        ResolveError::SequenceTooDeep {
            action: action.clone()
        }
        .to_string(),
        "action core:new-pane nests past the maximum of 8 sequence levels"
    );
}

#[test]
fn resolve_error_is_a_recoverable_config_error() {
    let error = ResolveError::Unregistered {
        action: core("new-pane"),
    };

    assert_eq!(error.category(), DomainCategory::Config);
    assert_eq!(error.severity(), Severity::Recoverable);
}

#[test]
fn coming_soon_status_is_checked_before_args_mismatch() {
    // `copy-mode-enter` is seeded `ComingSoon` and takes `ActionArgs::None`.
    // The status check runs before the handler is even matched on, so a
    // shape that would otherwise be an `ArgsMismatch` still reports
    // `ComingSoon` first.
    let registry = ActionRegistry::new();
    let action = core("copy-mode-enter");
    let wrong_args = ActionArgs::Run {
        program: run_program(),
        args: vec![],
        direction: None,
        stacked: false,
    };

    assert_eq!(
        resolve_action(&action, &wrong_args, &registry),
        Err(ResolveError::ComingSoon {
            action: action.clone()
        })
    );
}

#[test]
fn coming_soon_status_is_checked_before_the_plugin_route() {
    // A plugin action seeded `ComingSoon` must still refuse with
    // `ComingSoon`, never routing through as a `PluginHostCall`: the status
    // check runs before the handler match.
    let owner = plugin_id(1);
    let action = ActionRef::plugin(owner, "open-status").expect("valid name");
    let mut metadata = plugin_metadata(owner);
    metadata.status = ActionStatus::ComingSoon;
    let mut registry = ActionRegistry::new();
    insert_unchecked(&mut registry, action.clone(), metadata);

    assert_eq!(
        resolve_action(&action, &ActionArgs::None, &registry),
        Err(ResolveError::ComingSoon { action })
    );
}

#[test]
fn an_empty_sequence_resolves_to_an_empty_plan() {
    let registry = registry_with_macro("noop", vec![]);
    let macro_ref = user("noop");

    assert_eq!(
        resolve_action(&macro_ref, &ActionArgs::None, &registry),
        Ok(DispatchPlan::Sequence(vec![]))
    );
}

#[test]
fn plugin_action_forwards_no_arguments_untouched() {
    // A plugin route accepts any `ActionArgs`, uninterpreted, including
    // `None` — there is no schema check on the resolver's side.
    let owner = plugin_id(1);
    let action = ActionRef::plugin(owner, "open-status").expect("valid name");
    let mut registry = ActionRegistry::new();
    registry
        .register(owner, action.clone(), plugin_metadata(owner))
        .expect("plugin registers its own action");

    assert_eq!(
        resolve_action(&action, &ActionArgs::None, &registry),
        Ok(DispatchPlan::PluginHostCall {
            plugin: owner,
            action,
            args: ActionArgs::None,
        })
    );
}

#[test]
fn an_unhandled_core_action_name_falls_through_to_args_mismatch() {
    // A `core:` entry whose name is not one of `resolve_core`'s match arms
    // (e.g. a seed added to the registry table without a matching resolver
    // arm) is refused as `ArgsMismatch`, not a panic or a silent no-op.
    let mut registry = ActionRegistry::new();
    let action = core("bogus-unhandled-action");
    insert_unchecked(
        &mut registry,
        action.clone(),
        ActionMetadata {
            namespace: ActionNamespace::Core,
            display_name: "Bogus".to_string(),
            description: "Not in the resolve_core table".to_string(),
            scope_class: ActionScope::Global,
            target_compat: vec![],
            args_schema: None,
            handler: ActionHandlerRef::CoreCommand(CommandKind::Quit),
            status: ActionStatus::Available,
            continuous: false,
        },
    );

    assert_eq!(
        resolve_action(&action, &ActionArgs::None, &registry),
        Err(ResolveError::ArgsMismatch { action })
    );
}

#[test]
fn command_kind_alone_cannot_pick_the_command() {
    let registry = ActionRegistry::new();
    let lock = registry.lookup(&core("lock")).expect("seeded");
    let unlock = registry.lookup(&core("unlock")).expect("seeded");

    assert_eq!(
        lock.handler,
        ActionHandlerRef::CoreCommand(CommandKind::SetLockMode)
    );
    assert_eq!(
        unlock.handler,
        ActionHandlerRef::CoreCommand(CommandKind::SetLockMode)
    );
    assert_ne!(
        resolve_action(&core("lock"), &ActionArgs::None, &registry),
        resolve_action(&core("unlock"), &ActionArgs::None, &registry),
    );
}
