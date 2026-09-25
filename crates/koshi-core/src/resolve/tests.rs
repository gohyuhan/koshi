//! Unit tests for action resolution: the built-in name table, the named
//! variants that distinguish actions sharing one command, argument fit, the
//! plugin and macro routes, and the orphan and coming-soon refusals.

use super::*;

use crate::action::{
    build_core_action_seeds, ActionMetadata, ActionNamespace, ActionScope, ClientActionKind,
    TargetKind,
};
use crate::command::CommandKind;
use crate::process::ShellKind;
use crate::registry::tests::insert_action_without_validation;
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

/// A `core:` reference for a name known to satisfy the grammar.
fn build_core_action_reference(action_name: &str) -> ActionReference {
    ActionReference::from_core_action_name(action_name).expect("test action name is valid")
}

/// A plugin id built from a fixed uuid, so the same byte yields the same plugin.
fn build_test_plugin_id(uuid_fill_byte: u8) -> PluginId {
    PluginId::from_uuid(Uuid::from_bytes([uuid_fill_byte; 16]))
}

/// The program `core:run` is exercised with.
fn build_run_program_path() -> PathBuf {
    PathBuf::from("/usr/bin/lazygit")
}

/// The spawn spec `core:run` must build from [`build_run_program_path`]: no working
/// directory, no environment variables, and a shell kind derived from the
/// program.
fn build_run_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: build_run_program_path(),
        arguments: vec!["--all".to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("lazygit".to_string()),
    }
}

/// The `Available` core actions that no binding can invoke: each takes a
/// required value that no binding supplies (a resize amount, a move
/// direction, a pane id, a tab index, or the text to type), so it is reachable only
/// through a CLI command, which builds its [`Command`] directly.
/// `resolve_action` refuses every one of them whatever the arguments.
/// Pinned against the seed table by [`available_action_table_matches_seeds`].
const CLI_ONLY: [&str; 8] = [
    "resize-pane",
    "focus-pane",
    "focus-tab",
    "move-tab",
    "move-pane",
    "place-pane",
    "scroll-pane",
    "write-to-pane",
];

/// Available viewer-local actions. They are resolved to [`DispatchPlan::ClientAction`]
/// and therefore do not belong to the command table below.
const CLIENT_ACTIONS: [&str; 14] = [
    "begin-pane-placement",
    "select-pane-target-left",
    "select-pane-target-down",
    "select-pane-target-up",
    "select-pane-target-right",
    "select-pane-insertion-left",
    "select-pane-insertion-down",
    "select-pane-insertion-up",
    "select-pane-insertion-right",
    "cycle-pane-placement-span",
    "select-next-placement-tab",
    "select-previous-placement-tab",
    "confirm-pane-placement",
    "cancel-pane-placement",
];

/// The `layout.new-pane-direction` the resolving client holds throughout this
/// module. It is not `Right`, so a row expecting it cannot pass on a hardcoded
/// stock default.
const CLIENT_SPLIT: Direction = Direction::Up;

/// A `new-pane` request carrying [`CLIENT_SPLIT`]: what `core:new-pane` builds
/// for a client on that setting.
fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source_pane_id: None,
        tab_id: None,
        direction: CLIENT_SPLIT,
        should_stack: false,
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// Every binding-invocable `Available` core action, the arguments it is
/// invoked with, and the exact command it must produce. Together with
/// [`CLI_ONLY`] this covers the whole `Available` seed set, pinned by
/// [`available_action_table_matches_seeds`], so an action that gains or loses
/// `Available` status without a matching row fails the suite.
fn build_available_action_table() -> Vec<(&'static str, ActionArgs, Command)> {
    vec![
        (
            "new-pane",
            ActionArgs::None,
            Command::NewPane(build_new_pane_args()),
        ),
        (
            "new-pane-left",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Left,
                should_stack: false,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-down",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Down,
                should_stack: false,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-up",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Up,
                should_stack: false,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-right",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
                should_stack: false,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-stacked",
            ActionArgs::None,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: CLIENT_SPLIT,
                should_stack: true,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
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
                pane_id: None,
                should_force_close: false,
                should_kill_process_tree: true,
            }),
        ),
        (
            "resize-pane-left",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Left,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-down",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Down,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-up",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Up,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-right",
            ActionArgs::None,
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Right,
                resize_amount_cells: 1,
            }),
        ),
        (
            "focus-pane-left",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Left),
                client_id: None,
            }),
        ),
        (
            "focus-pane-down",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Down),
                client_id: None,
            }),
        ),
        (
            "focus-pane-up",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Up),
                client_id: None,
            }),
        ),
        (
            "focus-pane-right",
            ActionArgs::None,
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Right),
                client_id: None,
            }),
        ),
        (
            "scroll-pane-up",
            ActionArgs::None,
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: 3,
            }),
        ),
        (
            "scroll-pane-down",
            ActionArgs::None,
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: -3,
            }),
        ),
        (
            "toggle-pane-fullscreen",
            ActionArgs::None,
            Command::TogglePaneFullscreen,
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
                focus_target: TabTarget::Next,
                client_id: None,
            }),
        ),
        (
            "previous-tab",
            ActionArgs::None,
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Prev,
                client_id: None,
            }),
        ),
        ("quit", ActionArgs::None, Command::Quit),
        (
            "toggle-lock",
            ActionArgs::None,
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
        (
            "lock",
            ActionArgs::None,
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
        ),
        (
            "unlock",
            ActionArgs::None,
            Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
            }),
        ),
        ("mouse-select", ActionArgs::None, Command::ToggleMouseSelect),
        (
            "run",
            ActionArgs::Run {
                program: build_run_program_path(),
                arguments: vec!["--all".to_string()],
                direction: Some(Direction::Down),
                should_stack: false,
            },
            Command::RunCommandPane(RunCommandPaneArgs {
                spawn_spec: build_run_spawn_spec(),
                working_directory: None,
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Down,
                should_stack: false,
                client_id: None,
            }),
        ),
    ]
}

/// Metadata a plugin's own registration carries: its namespace, and a handler
/// routing back to itself.
fn build_plugin_metadata(plugin_id: PluginId) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::Plugin(plugin_id),
        display_name: "Open Status".to_string(),
        description: "Open the status view".to_string(),
        scope: ActionScope::Global,
        target_kinds: vec![TargetKind::Session],
        handler: ActionHandlerReference::PluginHostCall(plugin_id),
        action_status: ActionStatus::Available,
        is_continuous: false,
    }
}

/// Metadata for a `user:` macro whose handler fires `action_steps` in order.
fn build_macro_metadata(action_steps: Vec<ActionReference>) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::User,
        display_name: "Macro".to_string(),
        description: "A user macro".to_string(),
        scope: ActionScope::Global,
        target_kinds: vec![TargetKind::Session],
        handler: ActionHandlerReference::Sequence(action_steps),
        action_status: ActionStatus::Available,
        is_continuous: false,
    }
}

/// A `user:` reference for a name known to satisfy the grammar.
fn build_user_action_reference(action_name: &str) -> ActionReference {
    ActionReference::from_user_action_name(action_name).expect("test macro name is valid")
}

/// A registry holding the core seeds plus one `user:` macro whose handler is the
/// given sequence. `register_action` refuses `user:` references, so the entry goes in
/// through [`insert_action_without_validation`].
fn build_registry_with_macro(
    action_name: &str,
    action_steps: Vec<ActionReference>,
) -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    insert_action_without_validation(
        &mut registry,
        build_user_action_reference(action_name),
        build_macro_metadata(action_steps),
    );
    registry
}

/// A registry holding a chain of `levels` nested macros — `m0` names `m1`, which
/// names `m2`, and so on — the innermost naming `core:lock`. Returns the
/// registry and the outermost reference.
///
/// The chain is what distinguishes counting sequences from counting
/// resolutions: it has `levels` sequence handlers and one leaf action beneath
/// them.
fn build_registry_with_macro_chain(macro_depth_count: usize) -> (ActionRegistry, ActionReference) {
    let mut registry = ActionRegistry::new();
    for macro_depth in 0..macro_depth_count {
        let action_step = if macro_depth + 1 == macro_depth_count {
            build_core_action_reference("lock")
        } else {
            build_user_action_reference(&format!("m{}", macro_depth + 1))
        };
        insert_action_without_validation(
            &mut registry,
            build_user_action_reference(&format!("m{macro_depth}")),
            build_macro_metadata(vec![action_step]),
        );
    }
    (registry, build_user_action_reference("m0"))
}

#[test]
fn available_action_table_matches_seeds() {
    let mut seeded_action_names: Vec<String> = build_core_action_seeds()
        .into_iter()
        .filter(|(_, action_metadata)| action_metadata.action_status == ActionStatus::Available)
        .map(|(action_reference, _)| action_reference.action_name.get_name().to_string())
        .collect();
    seeded_action_names.sort();

    let mut tabled_action_names: Vec<String> = build_available_action_table()
        .into_iter()
        .map(|(action_name, _, _)| action_name.to_string())
        .chain(CLI_ONLY.into_iter().map(str::to_string))
        .chain(CLIENT_ACTIONS.into_iter().map(str::to_string))
        .collect();
    tabled_action_names.sort();

    assert_eq!(seeded_action_names, tabled_action_names);
}

#[test]
fn every_available_action_resolves_to_its_exact_command() {
    let registry = ActionRegistry::new();
    for (action_name, action_arguments, expected_command) in build_available_action_table() {
        let plan = resolve_action(
            &build_core_action_reference(action_name),
            &action_arguments,
            &registry,
            CLIENT_SPLIT,
        )
        .unwrap_or_else(|resolve_error| {
            panic!("core:{action_name} must resolve, got {resolve_error}")
        });
        assert_eq!(
            plan,
            DispatchPlan::Command(expected_command),
            "core:{action_name}"
        );
    }
}

#[test]
fn begin_pane_placement_resolves_to_the_viewer_local_action() {
    let registry = ActionRegistry::new();
    let begin_pane_placement_action = build_core_action_reference("begin-pane-placement");

    assert_eq!(
        resolve_action(
            &begin_pane_placement_action,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Ok(DispatchPlan::ClientAction(
            ClientActionKind::BeginPanePlacement
        ))
    );
}

#[test]
fn scroll_action_arguments_override_the_viewer_default() {
    let registry = ActionRegistry::new();
    let scroll_up = build_core_action_reference("scroll-pane-up");
    let scroll_down = build_core_action_reference("scroll-pane-down");

    assert_eq!(
        resolve_action_with_scroll_line_count(
            &scroll_up,
            &ActionArgs::Scroll {
                scroll_line_count: Some(7),
            },
            &registry,
            CLIENT_SPLIT,
            11,
        ),
        Ok(DispatchPlan::Command(Command::ScrollPane(ScrollPaneArgs {
            pane_id: None,
            scroll_line_count: 7,
        })))
    );
    assert_eq!(
        resolve_action_with_scroll_line_count(
            &scroll_down,
            &ActionArgs::Scroll {
                scroll_line_count: Some(7),
            },
            &registry,
            CLIENT_SPLIT,
            11,
        ),
        Ok(DispatchPlan::Command(Command::ScrollPane(ScrollPaneArgs {
            pane_id: None,
            scroll_line_count: -7,
        })))
    );
    assert_eq!(
        resolve_action_with_scroll_line_count(
            &scroll_down,
            &ActionArgs::Scroll {
                scroll_line_count: None,
            },
            &registry,
            CLIENT_SPLIT,
            11,
        ),
        Ok(DispatchPlan::Command(Command::ScrollPane(ScrollPaneArgs {
            pane_id: None,
            scroll_line_count: -11,
        })))
    );
}

#[test]
fn resolved_command_kind_matches_the_seeded_handler() {
    let registry = ActionRegistry::new();
    for (action_name, action_arguments, _) in build_available_action_table() {
        let action_reference = build_core_action_reference(action_name);
        let action_metadata = registry
            .find_action_metadata(&action_reference)
            .expect("seed is registered");
        let ActionHandlerReference::CoreCommand(command_kind) = action_metadata.handler else {
            panic!("core:{action_name} must dispatch a core command");
        };
        let Ok(DispatchPlan::Command(command)) = resolve_action(
            &action_reference,
            &action_arguments,
            &registry,
            CLIENT_SPLIT,
        ) else {
            panic!("core:{action_name} must resolve to a command");
        };
        assert_eq!(
            command.get_command_kind(),
            command_kind,
            "core:{action_name}"
        );
    }
}

#[test]
fn coming_soon_actions_are_refused() {
    let registry = ActionRegistry::new();
    let coming_soon_action_references: Vec<ActionReference> = build_core_action_seeds()
        .into_iter()
        .filter(|(_, action_metadata)| action_metadata.action_status == ActionStatus::ComingSoon)
        .map(|(action_reference, _)| action_reference)
        .collect();

    assert_eq!(coming_soon_action_references.len(), 7);
    for action_reference in coming_soon_action_references {
        assert_eq!(
            resolve_action(
                &action_reference,
                &ActionArgs::None,
                &registry,
                CLIENT_SPLIT,
            ),
            Err(ResolveError::ComingSoon {
                action_reference: action_reference.clone()
            }),
            "{action_reference}"
        );
    }
}

#[test]
fn coming_soon_names_are_pinned() {
    let mut coming_soon_action_names: Vec<String> = build_core_action_seeds()
        .into_iter()
        .filter(|(_, action_metadata)| action_metadata.action_status == ActionStatus::ComingSoon)
        .map(|(action_reference, _)| action_reference.action_name.get_name().to_string())
        .collect();
    coming_soon_action_names.sort();

    assert_eq!(
        coming_soon_action_names,
        vec![
            "copy-selection",
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
    let action_reference =
        ActionReference::from_plugin_action_name(build_test_plugin_id(1), "open-status")
            .expect("valid name");

    assert_eq!(
        resolve_action(
            &action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Err(ResolveError::Unregistered {
            action_reference: action_reference.clone()
        })
    );
}

#[test]
fn plugin_action_routes_to_its_own_host_call() {
    let plugin_id = build_test_plugin_id(1);
    let action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid name");
    let mut registry = ActionRegistry::new();
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_metadata(plugin_id),
        )
        .expect("plugin registers its own action");

    let action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec![],
        direction: None,
        should_stack: false,
    };
    assert_eq!(
        resolve_action(
            &action_reference,
            &action_arguments,
            &registry,
            CLIENT_SPLIT,
        ),
        Ok(DispatchPlan::PluginHostCall {
            plugin_id,
            action_reference,
            action_arguments,
        })
    );
}

#[test]
fn cli_only_actions_are_refused_with_and_without_arguments() {
    // `run` joins the CLI-only set for the argless half: a bare `core:run`
    // names no program and rejects `None` like the others. It accepts its own
    // `Run` arguments, covered by the available table.
    let registry = ActionRegistry::new();
    let some_action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec![],
        direction: None,
        should_stack: false,
    };
    for action_name in CLI_ONLY {
        let action_reference = build_core_action_reference(action_name);
        assert_eq!(
            resolve_action(
                &action_reference,
                &ActionArgs::None,
                &registry,
                CLIENT_SPLIT,
            ),
            Err(ResolveError::ArgsMismatch {
                action_reference: action_reference.clone()
            }),
            "core:{action_name} given no arguments"
        );
        assert_eq!(
            resolve_action(
                &action_reference,
                &some_action_arguments,
                &registry,
                CLIENT_SPLIT,
            ),
            Err(ResolveError::ArgsMismatch {
                action_reference: action_reference.clone()
            }),
            "core:{action_name} given arguments"
        );
    }
    let run_action_reference = build_core_action_reference("run");
    assert_eq!(
        resolve_action(
            &run_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::ArgsMismatch {
            action_reference: run_action_reference
        })
    );
}

#[test]
fn arguments_belonging_to_another_action_are_refused() {
    let registry = ActionRegistry::new();
    let action_reference = build_core_action_reference("next-tab");

    assert_eq!(
        resolve_action(
            &action_reference,
            &ActionArgs::Run {
                program: build_run_program_path(),
                arguments: vec![],
                direction: None,
                should_stack: false,
            },
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::ArgsMismatch {
            action_reference: action_reference.clone()
        })
    );
}

#[test]
fn a_sequence_resolves_each_step_in_order() {
    let registry = build_registry_with_macro(
        "split-and-lock",
        vec![
            build_core_action_reference("new-pane"),
            build_core_action_reference("lock"),
        ],
    );
    let macro_action_reference = build_user_action_reference("split-and-lock");

    assert_eq!(
        resolve_action(
            &macro_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Ok(DispatchPlan::Sequence(vec![
            DispatchPlan::Command(Command::NewPane(build_new_pane_args())),
            DispatchPlan::Command(Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None
            })),
        ]))
    );
}

#[test]
fn a_sequence_halts_on_the_first_failing_step() {
    let registry = build_registry_with_macro(
        "lock-then-copy",
        vec![
            build_core_action_reference("lock"),
            build_core_action_reference("copy-selection"),
            build_core_action_reference("unlock"),
        ],
    );
    let macro_action_reference = build_user_action_reference("lock-then-copy");

    assert_eq!(
        resolve_action(
            &macro_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::ComingSoon {
            action_reference: build_core_action_reference("copy-selection"),
        })
    );
}

#[test]
fn a_sequence_given_arguments_is_refused() {
    let registry = build_registry_with_macro(
        "split-and-lock",
        vec![build_core_action_reference("new-pane")],
    );
    let macro_action_reference = build_user_action_reference("split-and-lock");

    assert_eq!(
        resolve_action(
            &macro_action_reference,
            &ActionArgs::Run {
                program: build_run_program_path(),
                arguments: vec![],
                direction: None,
                should_stack: false,
            },
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::ArgsMismatch {
            action_reference: macro_action_reference.clone()
        })
    );
}

#[test]
fn a_self_referencing_macro_exhausts_the_depth_budget() {
    let macro_action_reference = build_user_action_reference("loop");
    let registry = build_registry_with_macro("loop", vec![macro_action_reference.clone()]);

    assert_eq!(
        resolve_action(
            &macro_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::SequenceTooDeep {
            action_reference: macro_action_reference.clone()
        })
    );
}

#[test]
fn a_chain_of_exactly_max_depth_sequences_resolves() {
    let (registry, outermost_action_reference) =
        build_registry_with_macro_chain(MAX_SEQUENCE_DEPTH);

    // The leaf action sits one level below the deepest sequence, and resolves:
    // the budget counts the sequences entered, not the actions reached.
    let mut dispatch_plan = resolve_action(
        &outermost_action_reference,
        &ActionArgs::None,
        &registry,
        CLIENT_SPLIT,
    )
    .expect("a chain at the documented limit must resolve");
    for _ in 0..MAX_SEQUENCE_DEPTH - 1 {
        let DispatchPlan::Sequence(mut dispatch_steps) = dispatch_plan else {
            panic!("every level but the last is a sequence");
        };
        assert_eq!(dispatch_steps.len(), 1);
        dispatch_plan = dispatch_steps.remove(0);
    }

    assert_eq!(
        dispatch_plan,
        DispatchPlan::Sequence(vec![DispatchPlan::Command(Command::SetLockMode(
            LockModeArgs {
                is_locked: true,
                client_id: None
            }
        ))])
    );
}

#[test]
fn a_chain_one_sequence_past_max_depth_is_refused() {
    let (registry, outermost_action_reference) =
        build_registry_with_macro_chain(MAX_SEQUENCE_DEPTH + 1);

    assert_eq!(
        resolve_action(
            &outermost_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Err(ResolveError::SequenceTooDeep {
            // The macro at the deepest allowed level is the one refused.
            action_reference: build_user_action_reference(&format!("m{MAX_SEQUENCE_DEPTH}")),
        })
    );
}

#[test]
fn run_never_carries_a_cwd_or_env_from_its_caller() {
    let registry = ActionRegistry::new();
    let run_action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec!["--all".to_string()],
        direction: None,
        should_stack: false,
    };

    let Ok(DispatchPlan::Command(Command::RunCommandPane(resolved_run_command_args))) =
        resolve_action(
            &build_core_action_reference("run"),
            &run_action_arguments,
            &registry,
            CLIENT_SPLIT,
        )
    else {
        panic!("core:run must resolve to a run-command-pane command");
    };

    assert_eq!(resolved_run_command_args.working_directory, None);
    assert_eq!(resolved_run_command_args.spawn_spec.working_directory, None);
    assert_eq!(
        resolved_run_command_args.spawn_spec.environment_variables,
        BTreeMap::new()
    );
    assert_eq!(
        resolved_run_command_args.spawn_spec.program,
        build_run_program_path()
    );
    assert_eq!(
        resolved_run_command_args.spawn_spec.arguments,
        vec!["--all".to_string()]
    );
    assert_eq!(
        resolved_run_command_args.spawn_spec.shell_kind,
        ShellKind::Other("lazygit".to_string())
    );
}

#[test]
fn resolve_error_messages_name_the_action() {
    let action_reference = build_core_action_reference("new-pane");

    assert_eq!(
        ResolveError::Unregistered {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action core:new-pane is not registered"
    );
    assert_eq!(
        ResolveError::ComingSoon {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action core:new-pane is not implemented yet"
    );
    assert_eq!(
        ResolveError::ArgsMismatch {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action core:new-pane was given arguments it does not accept"
    );
    assert_eq!(
        ResolveError::SequenceTooDeep {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action core:new-pane nests past the maximum of 8 sequence levels"
    );
}

#[test]
fn resolve_error_is_a_recoverable_config_error() {
    let resolve_error = ResolveError::Unregistered {
        action_reference: build_core_action_reference("new-pane"),
    };

    assert_eq!(resolve_error.category(), DomainCategory::Config);
    assert_eq!(resolve_error.get_severity(), Severity::Recoverable);
}

#[test]
fn coming_soon_status_is_checked_before_args_mismatch() {
    // `copy-selection` is seeded `ComingSoon` and takes `ActionArgs::None`.
    // The status check runs before the handler match: an argument shape
    // `resolve_core_action` refuses as `ArgsMismatch` reports `ComingSoon`.
    let registry = ActionRegistry::new();
    let action_reference = build_core_action_reference("copy-selection");
    let wrong_action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec![],
        direction: None,
        should_stack: false,
    };

    assert_eq!(
        resolve_action(
            &action_reference,
            &wrong_action_arguments,
            &registry,
            CLIENT_SPLIT,
        ),
        Err(ResolveError::ComingSoon {
            action_reference: action_reference.clone()
        })
    );
}

#[test]
fn coming_soon_status_is_checked_before_the_plugin_route() {
    // A plugin action seeded `ComingSoon` must still refuse with
    // `ComingSoon`, never routing through as a `PluginHostCall`: the status
    // check runs before the handler match.
    let plugin_id = build_test_plugin_id(1);
    let action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid name");
    let mut action_metadata = build_plugin_metadata(plugin_id);
    action_metadata.action_status = ActionStatus::ComingSoon;
    let mut registry = ActionRegistry::new();
    insert_action_without_validation(&mut registry, action_reference.clone(), action_metadata);

    assert_eq!(
        resolve_action(
            &action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Err(ResolveError::ComingSoon { action_reference })
    );
}

#[test]
fn an_empty_sequence_resolves_to_an_empty_plan() {
    let registry = build_registry_with_macro("noop", vec![]);
    let macro_action_reference = build_user_action_reference("noop");

    assert_eq!(
        resolve_action(
            &macro_action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Ok(DispatchPlan::Sequence(vec![]))
    );
}

#[test]
fn plugin_action_forwards_no_arguments_untouched() {
    // A plugin route accepts any `ActionArgs`, uninterpreted, including
    // `None` — there is no schema check on the resolver's side.
    let plugin_id = build_test_plugin_id(1);
    let action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid name");
    let mut registry = ActionRegistry::new();
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_metadata(plugin_id),
        )
        .expect("plugin registers its own action");

    assert_eq!(
        resolve_action(
            &action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Ok(DispatchPlan::PluginHostCall {
            plugin_id,
            action_reference,
            action_arguments: ActionArgs::None,
        })
    );
}

#[test]
fn an_unhandled_core_action_name_falls_through_to_args_mismatch() {
    // A `core:` entry whose name is not one of `resolve_core_action`'s match arms
    // (e.g. a seed added to the registry table without a matching resolver
    // arm) is refused as `ArgsMismatch`, not a panic or a silent no-op.
    let mut registry = ActionRegistry::new();
    let action_reference = build_core_action_reference("bogus-unhandled-action");
    insert_action_without_validation(
        &mut registry,
        action_reference.clone(),
        ActionMetadata {
            namespace: ActionNamespace::Core,
            display_name: "Bogus".to_string(),
            description: "Not in the resolve_core_action table".to_string(),
            scope: ActionScope::Global,
            target_kinds: vec![],
            handler: ActionHandlerReference::CoreCommand(CommandKind::Quit),
            action_status: ActionStatus::Available,
            is_continuous: false,
        },
    );

    assert_eq!(
        resolve_action(
            &action_reference,
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT,
        ),
        Err(ResolveError::ArgsMismatch { action_reference })
    );
}

#[test]
fn command_kind_alone_cannot_pick_the_command() {
    let registry = ActionRegistry::new();
    let lock_action_metadata = registry
        .find_action_metadata(&build_core_action_reference("lock"))
        .expect("seeded");
    let unlock_action_metadata = registry
        .find_action_metadata(&build_core_action_reference("unlock"))
        .expect("seeded");

    assert_eq!(
        lock_action_metadata.handler,
        ActionHandlerReference::CoreCommand(CommandKind::SetLockMode)
    );
    assert_eq!(
        unlock_action_metadata.handler,
        ActionHandlerReference::CoreCommand(CommandKind::SetLockMode)
    );
    assert_ne!(
        resolve_action(
            &build_core_action_reference("lock"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        resolve_action(
            &build_core_action_reference("unlock"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
    );
}

#[test]
fn run_without_a_direction_splits_toward_the_client_setting() {
    let registry = ActionRegistry::new();
    let run_action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec![],
        direction: None,
        should_stack: false,
    };

    assert_eq!(
        resolve_action(
            &build_core_action_reference("run"),
            &run_action_arguments,
            &registry,
            CLIENT_SPLIT,
        ),
        Ok(DispatchPlan::Command(Command::RunCommandPane(
            RunCommandPaneArgs {
                spawn_spec: SpawnSpec {
                    program: build_run_program_path(),
                    arguments: vec![],
                    working_directory: None,
                    environment_variables: BTreeMap::new(),
                    shell_kind: ShellKind::Other("lazygit".to_string()),
                },
                working_directory: None,
                source_pane_id: None,
                tab_id: None,
                direction: CLIENT_SPLIT,
                should_stack: false,
                client_id: None,
            }
        )))
    );
}

#[test]
fn run_stacked_builds_a_stacked_pane_and_still_carries_the_direction() {
    let registry = ActionRegistry::new();
    let run_action_arguments = ActionArgs::Run {
        program: build_run_program_path(),
        arguments: vec![],
        direction: Some(Direction::Left),
        should_stack: true,
    };

    let Ok(DispatchPlan::Command(Command::RunCommandPane(resolved_run_command_args))) =
        resolve_action(
            &build_core_action_reference("run"),
            &run_action_arguments,
            &registry,
            CLIENT_SPLIT,
        )
    else {
        panic!("core:run must resolve to a run-command-pane command");
    };

    assert!(resolved_run_command_args.should_stack);
    assert_eq!(resolved_run_command_args.direction, Direction::Left);
}

#[test]
fn run_classifies_a_known_shell_from_its_program() {
    let registry = ActionRegistry::new();
    let run_action_arguments = ActionArgs::Run {
        program: PathBuf::from("/bin/zsh"),
        arguments: vec!["-l".to_string()],
        direction: None,
        should_stack: false,
    };

    let Ok(DispatchPlan::Command(Command::RunCommandPane(resolved_run_command_args))) =
        resolve_action(
            &build_core_action_reference("run"),
            &run_action_arguments,
            &registry,
            CLIENT_SPLIT,
        )
    else {
        panic!("core:run must resolve to a run-command-pane command");
    };

    assert_eq!(
        resolved_run_command_args.spawn_spec.program,
        PathBuf::from("/bin/zsh")
    );
    assert_eq!(
        resolved_run_command_args.spawn_spec.arguments,
        vec!["-l".to_string()]
    );
    assert_eq!(
        resolved_run_command_args.spawn_spec.shell_kind,
        ShellKind::Zsh
    );
}

#[test]
fn a_sequence_step_naming_a_plugin_action_routes_to_its_host_call_with_no_arguments() {
    let plugin_id = build_test_plugin_id(1);
    let plugin_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid name");
    let mut registry = build_registry_with_macro(
        "lock-and-open",
        vec![
            build_core_action_reference("lock"),
            plugin_action_reference.clone(),
        ],
    );
    registry
        .register_action(
            plugin_id,
            plugin_action_reference.clone(),
            build_plugin_metadata(plugin_id),
        )
        .expect("plugin registers its own action");

    assert_eq!(
        resolve_action(
            &build_user_action_reference("lock-and-open"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Ok(DispatchPlan::Sequence(vec![
            DispatchPlan::Command(Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None
            })),
            DispatchPlan::PluginHostCall {
                plugin_id,
                action_reference: plugin_action_reference,
                action_arguments: ActionArgs::None,
            },
        ]))
    );
}

#[test]
fn a_sequence_naming_an_unregistered_step_reports_that_step() {
    let missing_action_reference =
        ActionReference::from_plugin_action_name(build_test_plugin_id(1), "open-status")
            .expect("valid name");
    let registry = build_registry_with_macro(
        "lock-and-open",
        vec![
            build_core_action_reference("lock"),
            missing_action_reference.clone(),
        ],
    );

    assert_eq!(
        resolve_action(
            &build_user_action_reference("lock-and-open"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::Unregistered {
            action_reference: missing_action_reference,
        })
    );
}

#[test]
fn a_sequence_step_naming_run_is_refused_for_lack_of_arguments() {
    // Every step resolves with `ActionArgs::None`, and `core:run` rejects it.
    let registry = build_registry_with_macro("run-it", vec![build_core_action_reference("run")]);

    assert_eq!(
        resolve_action(
            &build_user_action_reference("run-it"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::ArgsMismatch {
            action_reference: build_core_action_reference("run")
        })
    );
}

#[test]
fn a_macro_inside_a_macro_resolves_to_a_nested_plan_in_step_order() {
    let mut registry = build_registry_with_macro(
        "inner",
        vec![
            build_core_action_reference("lock"),
            build_core_action_reference("unlock"),
        ],
    );
    insert_action_without_validation(
        &mut registry,
        build_user_action_reference("outer"),
        build_macro_metadata(vec![
            build_core_action_reference("new-pane"),
            build_user_action_reference("inner"),
            build_core_action_reference("quit"),
        ]),
    );

    assert_eq!(
        resolve_action(
            &build_user_action_reference("outer"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Ok(DispatchPlan::Sequence(vec![
            DispatchPlan::Command(Command::NewPane(build_new_pane_args())),
            DispatchPlan::Sequence(vec![
                DispatchPlan::Command(Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: None
                })),
                DispatchPlan::Command(Command::SetLockMode(LockModeArgs {
                    is_locked: false,
                    client_id: None
                })),
            ]),
            DispatchPlan::Command(Command::Quit),
        ]))
    );
}

#[test]
fn two_macros_naming_each_other_exhaust_the_depth_budget() {
    // `ping` → `pong` → `ping` → … : depth 0 is `ping`, depth 1 is `pong`, and
    // every even depth is `ping` again. `MAX_SEQUENCE_DEPTH` is even, so the
    // macro refused at that depth is `ping`.
    let mut registry = build_registry_with_macro("ping", vec![build_user_action_reference("pong")]);
    insert_action_without_validation(
        &mut registry,
        build_user_action_reference("pong"),
        build_macro_metadata(vec![build_user_action_reference("ping")]),
    );

    assert_eq!(MAX_SEQUENCE_DEPTH % 2, 0);
    assert_eq!(
        resolve_action(
            &build_user_action_reference("ping"),
            &ActionArgs::None,
            &registry,
            CLIENT_SPLIT
        ),
        Err(ResolveError::SequenceTooDeep {
            action_reference: build_user_action_reference("ping")
        })
    );
}

#[test]
fn action_args_serialize_to_their_wire_form() {
    assert_eq!(
        serde_json::to_string(&ActionArgs::None).expect("serializes"),
        r#""None""#
    );
    assert_eq!(
        serde_json::from_str::<ActionArgs>(r#""None""#).expect("deserializes"),
        ActionArgs::None
    );
    let scroll_action_arguments = ActionArgs::Scroll {
        scroll_line_count: Some(-4),
    };
    assert_eq!(
        serde_json::to_string(&scroll_action_arguments).expect("serializes"),
        r#"{"Scroll":{"scroll_line_count":-4}}"#
    );
    assert_eq!(
        serde_json::from_str::<ActionArgs>(r#"{"Scroll":{"scroll_line_count":null}}"#)
            .expect("deserializes"),
        ActionArgs::Scroll {
            scroll_line_count: None,
        }
    );

    let run_action_arguments = ActionArgs::Run {
        program: PathBuf::from("/usr/bin/lazygit"),
        arguments: vec!["--all".to_string()],
        direction: Some(Direction::Down),
        should_stack: false,
    };
    let action_args_json = r#"{"Run":{"program":"/usr/bin/lazygit","arguments":["--all"],"direction":"Down","should_stack":false}}"#;
    assert_eq!(
        serde_json::to_string(&run_action_arguments).expect("serializes"),
        action_args_json
    );
    assert_eq!(
        serde_json::from_str::<ActionArgs>(action_args_json).expect("deserializes"),
        run_action_arguments
    );
}

#[test]
fn action_args_run_deserializes_a_null_direction() {
    let null_direction_action_args_json =
        r#"{"Run":{"program":"/bin/zsh","arguments":[],"direction":null,"should_stack":true}}"#;

    assert_eq!(
        serde_json::from_str::<ActionArgs>(null_direction_action_args_json).expect("deserializes"),
        ActionArgs::Run {
            program: PathBuf::from("/bin/zsh"),
            arguments: vec![],
            direction: None,
            should_stack: true,
        }
    );
}
