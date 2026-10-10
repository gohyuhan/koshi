//! Unit tests for action resolution: the built-in name table, the named
//! variants that distinguish actions sharing one command, the refusal of
//! actions that need CLI arguments, and the unregistered refusal.

use super::*;

use crate::action::{build_core_action_seeds, ClientActionKind};
use crate::command::CommandKind;

/// A `core:` reference for a name known to satisfy the grammar.
fn build_core_action_reference(action_name: &str) -> ActionReference {
    ActionReference::from_core_action_name(action_name).expect("test action name is valid")
}

/// The core actions that no binding can invoke: each takes a required value
/// that no binding supplies (a resize amount, a move direction, a pane id, a
/// tab index, the text to type, or the program to run), so it is reachable
/// only through a CLI command, which builds its [`Command`] directly.
/// `resolve_action` refuses every one of them. Pinned against the seed table
/// by [`action_tables_match_seeds`].
const CLI_ONLY: [&str; 9] = [
    "resize-pane",
    "focus-pane",
    "focus-tab",
    "move-tab",
    "move-pane",
    "place-pane",
    "run",
    "scroll-pane",
    "write-to-pane",
];

/// The viewer-local actions. They are resolved to [`DispatchPlan::ClientAction`]
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
/// module: `Up`, which differs from the stock default `Right`.
const CLIENT_SPLIT_DIRECTION: Direction = Direction::Up;

/// A `new-pane` request carrying [`CLIENT_SPLIT_DIRECTION`]: what `core:new-pane` builds
/// for a client on that setting.
fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: CLIENT_SPLIT_DIRECTION,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// Every binding-invocable core command action and the exact command it must
/// produce. Together with [`CLI_ONLY`] and
/// [`CLIENT_ACTIONS`] this covers the whole seed set, pinned by
/// [`action_tables_match_seeds`], so a seed added without a matching row fails
/// the suite.
fn build_bindable_action_table() -> Vec<(&'static str, Command)> {
    vec![
        ("new-pane", Command::NewPane(build_new_pane_args())),
        (
            "new-pane-left",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Left,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-down",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Down,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-up",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Up,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-right",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Right,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            "new-pane-stacked",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Stacked {
                    source_pane_id: None,
                    tab_id: None,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        ("close-pane", Command::ClosePane(ClosePaneArgs::default())),
        (
            "close-pane-tree",
            Command::ClosePane(ClosePaneArgs {
                pane_id: None,
                should_force_close: false,
                should_kill_process_tree: true,
            }),
        ),
        (
            "resize-pane-left",
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Left,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-down",
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Down,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-up",
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Up,
                resize_amount_cells: 1,
            }),
        ),
        (
            "resize-pane-right",
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Right,
                resize_amount_cells: 1,
            }),
        ),
        (
            "focus-pane-left",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Left),
                client_id: None,
            }),
        ),
        (
            "focus-pane-down",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Down),
                client_id: None,
            }),
        ),
        (
            "focus-pane-up",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Up),
                client_id: None,
            }),
        ),
        (
            "focus-pane-right",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(Direction::Right),
                client_id: None,
            }),
        ),
        (
            "focus-next-floating-pane",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::NextFloatingPane,
                client_id: None,
            }),
        ),
        (
            "focus-previous-floating-pane",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::PreviousFloatingPane,
                client_id: None,
            }),
        ),
        (
            "scroll-pane-up",
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: 3,
            }),
        ),
        (
            "scroll-pane-down",
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: -3,
            }),
        ),
        ("toggle-pane-fullscreen", Command::TogglePaneFullscreen),
        ("new-tab", Command::NewTab(NewTabArgs::default())),
        ("close-tab", Command::CloseTab(CloseTabArgs::default())),
        (
            "next-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            }),
        ),
        (
            "previous-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Previous,
                client_id: None,
            }),
        ),
        ("quit", Command::Quit),
        (
            "toggle-lock",
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
        ),
        (
            "lock",
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
        ),
        (
            "unlock",
            Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
            }),
        ),
        ("mouse-select", Command::ToggleMouseSelect),
    ]
}

#[test]
fn action_tables_match_seeds() {
    let mut seeded_action_names: Vec<String> = build_core_action_seeds()
        .into_iter()
        .map(|(action_reference, _)| action_reference.action_name.get_name().to_string())
        .collect();
    seeded_action_names.sort();

    let mut tabled_action_names: Vec<String> = build_bindable_action_table()
        .into_iter()
        .map(|(action_name, _)| action_name.to_string())
        .chain(CLI_ONLY.into_iter().map(str::to_string))
        .chain(CLIENT_ACTIONS.into_iter().map(str::to_string))
        .collect();
    tabled_action_names.sort();

    assert_eq!(seeded_action_names, tabled_action_names);
}

#[test]
fn every_bindable_action_resolves_to_its_exact_command() {
    let registry = ActionRegistry::new();
    for (action_name, expected_command) in build_bindable_action_table() {
        let plan = resolve_action(
            &build_core_action_reference(action_name),
            &registry,
            CLIENT_SPLIT_DIRECTION,
        )
        .unwrap_or_else(|resolve_error| {
            panic!("core:{action_name} must resolve, got {resolve_error}")
        });
        assert_eq!(
            plan,
            DispatchPlan::Command(Box::new(expected_command)),
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
            &registry,
            CLIENT_SPLIT_DIRECTION
        ),
        Ok(DispatchPlan::ClientAction(
            ClientActionKind::BeginPanePlacement
        ))
    );
}

#[test]
fn scroll_actions_scroll_the_caller_line_count() {
    let registry = ActionRegistry::new();

    assert_eq!(
        resolve_action_with_scroll_line_count(
            &build_core_action_reference("scroll-pane-up"),
            &registry,
            CLIENT_SPLIT_DIRECTION,
            11,
        ),
        Ok(DispatchPlan::Command(Box::new(Command::ScrollPane(
            ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: 11,
            }
        ))))
    );
    assert_eq!(
        resolve_action_with_scroll_line_count(
            &build_core_action_reference("scroll-pane-down"),
            &registry,
            CLIENT_SPLIT_DIRECTION,
            11,
        ),
        Ok(DispatchPlan::Command(Box::new(Command::ScrollPane(
            ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: -11,
            }
        ))))
    );
}
/// The [`CommandKind`] that names `command`, or `None` for a command no action
/// builds.
fn find_command_kind(command: &Command) -> Option<CommandKind> {
    match command {
        Command::NewPane(_) => Some(CommandKind::NewPane),
        Command::ClosePane(_) => Some(CommandKind::ClosePane),
        Command::ResizePane(_) => Some(CommandKind::ResizePane),
        Command::FocusPane(_) => Some(CommandKind::FocusPane),
        Command::NewTab(_) => Some(CommandKind::NewTab),
        Command::CloseTab(_) => Some(CommandKind::CloseTab),
        Command::FocusTab(_) => Some(CommandKind::FocusTab),
        Command::WriteToPane(_) => Some(CommandKind::WriteToPane),
        Command::ToggleLockMode(_) => Some(CommandKind::ToggleLockMode),
        Command::SetLockMode(_) => Some(CommandKind::SetLockMode),
        Command::ToggleMouseSelect => Some(CommandKind::ToggleMouseSelect),
        Command::TogglePaneFullscreen => Some(CommandKind::TogglePaneFullscreen),
        Command::MoveTab(_) => Some(CommandKind::MoveTab),
        Command::MovePane(_) => Some(CommandKind::MovePane),
        Command::PlacePane(_) => Some(CommandKind::PlacePane),
        Command::ScrollPane(_) => Some(CommandKind::ScrollPane),
        Command::Quit => Some(CommandKind::Quit),
        Command::Visual(_)
        | Command::MoveFloatingPane(_)
        | Command::SetPanePinned(_)
        | Command::SetPaneMinimized(_)
        | Command::SetAllFloatingPanesMinimized(_)
        | Command::Detach(_)
        | Command::DetachAll
        | Command::SwitchSession(_) => None,
    }
}

#[test]
fn resolved_command_kind_matches_the_seeded_handler() {
    let registry = ActionRegistry::new();
    for (action_name, _) in build_bindable_action_table() {
        let action_reference = build_core_action_reference(action_name);
        let action_metadata = registry
            .find_action_metadata(&action_reference)
            .expect("seed is registered");
        let ActionHandlerReference::CoreCommand(command_kind) = action_metadata.handler else {
            panic!("core:{action_name} must dispatch a core command");
        };
        let Ok(DispatchPlan::Command(command)) =
            resolve_action(&action_reference, &registry, CLIENT_SPLIT_DIRECTION)
        else {
            panic!("core:{action_name} must resolve to a command");
        };
        assert_eq!(
            find_command_kind(&command),
            Some(command_kind),
            "core:{action_name}"
        );
    }
}

#[test]
fn cli_only_actions_are_refused_as_needing_arguments() {
    let registry = ActionRegistry::new();
    for action_name in CLI_ONLY {
        let action_reference = build_core_action_reference(action_name);
        assert_eq!(
            resolve_action(&action_reference, &registry, CLIENT_SPLIT_DIRECTION),
            Err(ResolveError::ArgumentsRequired {
                action_reference: action_reference.clone()
            }),
            "core:{action_name}"
        );
    }
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
        ResolveError::ArgumentsRequired { action_reference }.to_string(),
        "action core:new-pane needs arguments only its CLI verb supplies"
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
            &registry,
            CLIENT_SPLIT_DIRECTION
        ),
        resolve_action(
            &build_core_action_reference("unlock"),
            &registry,
            CLIENT_SPLIT_DIRECTION
        ),
    );
}

#[test]
fn an_unseeded_action_is_unregistered() {
    let registry = ActionRegistry::new();
    let action_reference = build_core_action_reference("open-status");

    assert_eq!(
        resolve_action(&action_reference, &registry, CLIENT_SPLIT_DIRECTION,),
        Err(ResolveError::Unregistered {
            action_reference: action_reference.clone()
        })
    );
}
