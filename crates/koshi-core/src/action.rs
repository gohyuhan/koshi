//! Action vocabulary — the stable user surface.
//!
//! An *action* is what a user binds a key to or types as a CLI subcommand. It
//! is not a [`Command`](crate::command::Command): a `Command` is the runtime's
//! internal mutation type, an action is the public name config files use. Each
//! action maps to a command or to a viewer-local action.
//!
//! Every action lives in the `core:` namespace. This file holds the
//! primitives — [`ActionReference`], [`ActionMetadata`],
//! [`ActionHandlerReference`], and the static [`build_core_action_seeds`]
//! table. [`ActionRegistry`](crate::registry::ActionRegistry) loads that table
//! for lookup.

use crate::command::CommandKind;
use crate::geometry::Direction;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The maximum length of an [`ActionName`], from the grammar
/// `^[a-z][a-z0-9-]{0,39}$` (1 leading letter + up to 39 trailing chars).
const MAX_ACTION_NAME_CHARACTER_COUNT: usize = 40;

/// Why a string is not a valid [`ActionName`].
///
/// Names follow `^[a-z][a-z0-9-]{0,39}$`: a lowercase-letter start, then up to
/// thirty-nine more lowercase letters, digits, or hyphens. The display name shown to
/// users is free-form and lives separately in [`ActionMetadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionNameError {
    /// The name was empty.
    Empty,
    /// The name exceeded `MAX_ACTION_NAME_CHARACTER_COUNT` characters.
    TooLong {
        /// The offending length.
        character_count: usize,
    },
    /// The first character was not an ASCII lowercase letter.
    InvalidStart {
        /// The offending leading character.
        invalid_character: char,
    },
    /// A character after the first was outside `[a-z0-9-]`.
    InvalidChar {
        /// The offending character.
        invalid_character: char,
    },
}

impl fmt::Display for ActionNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionNameError::Empty => formatter.write_str("action name is empty"),
            ActionNameError::TooLong { character_count } => write!(
                formatter,
                "action name is {character_count} chars; the maximum is {MAX_ACTION_NAME_CHARACTER_COUNT}"
            ),
            ActionNameError::InvalidStart { invalid_character } => write!(
                formatter,
                "action name must start with a lowercase letter, found {invalid_character:?}"
            ),
            ActionNameError::InvalidChar { invalid_character } => {
                write!(
                    formatter,
                    "action name may only contain [a-z0-9-], found {invalid_character:?}"
                )
            }
        }
    }
}

impl std::error::Error for ActionNameError {}

/// The local name of an action within its namespace, validated against
/// `^[a-z][a-z0-9-]{0,39}$`.
///
/// [`ActionName::parse_action_name`] and deserialization (via
/// [`TryFrom<String>`]) both run the grammar check: a name decoded from a
/// config file or the IPC socket is always valid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionName(String);

impl ActionName {
    /// Parse and validate `action_name` against the action-name grammar.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] describing the first rule the input
    /// violates.
    pub fn parse_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        let mut remaining_characters = action_name.chars();
        let first_character = remaining_characters.next().ok_or(ActionNameError::Empty)?;
        if !first_character.is_ascii_lowercase() {
            return Err(ActionNameError::InvalidStart {
                invalid_character: first_character,
            });
        }
        // Every character after the first must be a lowercase letter, digit, or hyphen.
        for character in remaining_characters {
            if !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-') {
                return Err(ActionNameError::InvalidChar {
                    invalid_character: character,
                });
            }
        }
        // The length check runs after the charset scan: a name that is both
        // over-long and holds a bad character reports the bad character.
        let character_count = action_name.chars().count();
        if character_count > MAX_ACTION_NAME_CHARACTER_COUNT {
            return Err(ActionNameError::TooLong { character_count });
        }
        Ok(ActionName(action_name.to_string()))
    }

    /// Borrow the validated name.
    #[must_use]
    pub fn get_name(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ActionName {
    type Error = ActionNameError;

    fn try_from(action_name: String) -> Result<Self, Self::Error> {
        ActionName::parse_action_name(&action_name)
    }
}

impl From<ActionName> for String {
    fn from(action_name: ActionName) -> Self {
        action_name.0
    }
}

impl fmt::Display for ActionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A reference to a built-in action.
///
/// `Display` renders the canonical wire form used everywhere an action is named
/// by string — config files and CLI output: `core:new-pane`. Serde reads and
/// writes that same string via [`FromStr`]: a keymap entry
/// `"<C-p>n" action="core:new-pane"` decodes to exactly this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionReference {
    /// The action's name after the `core:` prefix.
    pub action_name: ActionName,
}

impl ActionReference {
    /// Build a reference to a built-in `core:` action.
    ///
    /// # Errors
    /// Returns an [`ActionNameError`] if `action_name` violates the grammar.
    pub fn from_core_action_name(action_name: &str) -> Result<Self, ActionNameError> {
        Ok(ActionReference {
            action_name: ActionName::parse_action_name(action_name)?,
        })
    }
}

impl fmt::Display for ActionReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "core:{}", self.action_name)
    }
}

/// Why a string is not a valid [`ActionReference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionReferenceParseError {
    /// No `namespace:` prefix was present.
    MissingNamespace,
    /// The namespace prefix was not `core`.
    UnknownNamespace {
        /// The unrecognized prefix.
        unknown_namespace: String,
    },
    /// The name after `core:` failed the action-name grammar.
    InvalidActionName(ActionNameError),
}

impl fmt::Display for ActionReferenceParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionReferenceParseError::MissingNamespace => {
                formatter.write_str("action reference is missing a 'namespace:' prefix")
            }
            ActionReferenceParseError::UnknownNamespace { unknown_namespace } => write!(
                formatter,
                "unknown action namespace {unknown_namespace:?}; expected core"
            ),
            ActionReferenceParseError::InvalidActionName(action_name_error) => {
                write!(formatter, "{action_name_error}")
            }
        }
    }
}

impl std::error::Error for ActionReferenceParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ActionReferenceParseError::InvalidActionName(action_name_error) => {
                Some(action_name_error)
            }
            _ => None,
        }
    }
}

impl FromStr for ActionReference {
    type Err = ActionReferenceParseError;

    fn from_str(action_reference_text: &str) -> Result<Self, Self::Err> {
        let (namespace, action_name) = action_reference_text
            .split_once(':')
            .ok_or(ActionReferenceParseError::MissingNamespace)?;
        if namespace != "core" {
            return Err(ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: namespace.to_string(),
            });
        }
        ActionReference::from_core_action_name(action_name)
            .map_err(ActionReferenceParseError::InvalidActionName)
    }
}

impl TryFrom<String> for ActionReference {
    type Error = ActionReferenceParseError;

    fn try_from(action_reference_text: String) -> Result<Self, Self::Error> {
        action_reference_text.parse()
    }
}

impl From<ActionReference> for String {
    fn from(action_reference: ActionReference) -> Self {
        action_reference.to_string()
    }
}

/// How broad an action's effect is. `koshi keys` and `koshi actions` output
/// print it as a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionScope {
    /// Acts on a pane within the current session.
    PaneSession,
    /// Acts on the issuing client (e.g. per-client view state).
    Client,
    /// Acts on a tab.
    Tab,
}

/// A kind of entity an action can target. [`ActionMetadata::target_kinds`]
/// lists the kinds an action accepts; `koshi actions explain` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// The session.
    Session,
    /// A tab.
    Tab,
    /// A pane.
    Pane,
    /// A client.
    Client,
}

/// How an action is dispatched once it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionHandlerReference {
    /// Build and dispatch the named core [`Command`](crate::command::Command).
    CoreCommand(CommandKind),
    /// Run a viewer-local action without sending a session command.
    CoreClient(ClientActionKind),
}

/// A core action handled by the attached viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientActionKind {
    /// Open persistent pane placement mode for the focused pane.
    BeginPanePlacement,
    /// Select the visible pane target in `direction` during pane placement mode.
    SelectPaneTarget(Direction),
    /// Select the insertion edge in `direction` during pane placement mode.
    SelectPaneInsertion(Direction),
    /// Cycle the available insertion spans during pane placement mode.
    CyclePanePlacementSpan,
    /// Preview the next visible tab during pane placement mode.
    SelectNextPlacementTab,
    /// Preview the previous visible tab during pane placement mode.
    SelectPreviousPlacementTab,
    /// Confirm the selected pane placement.
    ConfirmPanePlacement,
    /// Cancel pane placement and return to the base input mode.
    CancelPanePlacement,
    /// Open pane placement mode for the focused pane of the active tab, also
    /// while a floating pane takes the viewer's input.
    BeginTiledPanePlacement,
    /// Open floating pane move mode on the focused floating pane, or on the
    /// front-most floating pane the viewer draws and did not pin.
    BeginFloatingPaneMove,
    /// Open floating pane move mode when a floating pane takes the viewer's
    /// input, and pane placement mode otherwise.
    ConfirmPaneMoveChoice,
    /// Close the pane move chooser and return to the base input mode.
    CancelPaneMoveChoice,
    /// Move the selected floating pane's draft one cell in `direction` during
    /// floating pane move mode.
    MoveFloatingPane(Direction),
    /// Select the next floating pane, in the order the panes became floating,
    /// during floating pane move mode.
    SelectNextFloatingPane,
    /// Select the previous floating pane, in the order the panes became
    /// floating, during floating pane move mode.
    SelectPreviousFloatingPane,
    /// Send the selected floating pane's draft as a floating pane move.
    ConfirmFloatingPaneMove,
    /// Discard the draft and leave floating pane move mode.
    CancelFloatingPaneMove,
    /// Open dock/undock mode with the pane that takes the viewer's input
    /// selected.
    BeginDockUndock,
    /// In dock/undock mode, float the selected tiled pane where it is drawn,
    /// or dock the selected floating pane at the selected target.
    ConfirmDockUndock,
    /// Leave dock/undock mode and return to the base input mode.
    CancelDockUndock,
    /// Switch the selected placement target between a swap with a pane, or an
    /// insertion beside a pane or a whole stack, and the stack of that pane or
    /// that stack.
    TogglePanePlacementStack,
    /// Open the floating pane tray when it is closed, and close it when it is
    /// open.
    ToggleFloatingPaneTray,
    /// Close the floating pane tray and return to the base input mode.
    CloseFloatingPaneTray,
    /// Select the tray tile one step in `direction` inside the tile grid.
    SelectTrayTile(Direction),
    /// Minimize the selected tray tile's floating pane when the viewer shows
    /// it, and restore it when the viewer minimized it. On the `+ new` tile,
    /// close the tray and create a floating pane.
    ActivateSelectedTrayTile,
    /// Open the notification center, which then holds the viewer's keys, when
    /// it is closed, and close it when it is open.
    ToggleNotificationCenter,
    /// Close the notification center and return to the base input mode.
    CloseNotificationCenter,
    /// Remove every entry from the viewer's notification center and close it.
    ClearNotifications,
    /// Select the notification center's card one entry newer than the
    /// selected one, and scroll the center to show it.
    SelectNewerNotification,
    /// Select the notification center's card one entry older than the
    /// selected one, and scroll the center to show it.
    SelectOlderNotification,
    /// Remove the selected card's entry from the viewer's notification
    /// center, and select the next older card, else the next newer one.
    DismissSelectedNotification,
}

/// Where an action's key sorts among the keys of its modifier group in the
/// hint bar: `Essential` keys first, then `Navigation`, then `Other`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum HintTier {
    /// Confirms, cancels or closes a mode, splits the focused pane in the
    /// configured direction, closes a pane, opens or closes a tab, or locks or
    /// unlocks the client.
    Essential,
    /// Moves, selects, resizes or splits in one direction, or steps to the
    /// next or previous tab or floating pane.
    Navigation,
    /// Every other action.
    #[default]
    Other,
}

/// Everything the registry knows about one action: how to show it, what it can
/// target, and how to dispatch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionMetadata {
    /// Human-facing name, e.g. "Create Pane to the Right".
    pub display_name: String,
    /// One-line description for `describe`/which-key output.
    pub description: String,
    /// How broad the action's effect is.
    pub scope: ActionScope,
    /// Entity kinds the action can target.
    pub target_kinds: Vec<TargetKind>,
    /// How the action is dispatched.
    pub handler: ActionHandlerReference,
    /// Whether the action repeats from a held prefix: fired from a
    /// multi-chord binding, the binding's prefix stays armed and the next
    /// chord alone fires again (`<C-s> <Left> <Left> <Left>` resizes three
    /// times). Declared per action here, never in a binding.
    pub is_continuous: bool,
    /// The hint bar's label for the action in place of `display_name`, or
    /// `None` to show `display_name`. The four actions of one direction family
    /// share one label: `focus-pane-left`, `-down`, `-up` and `-right` all read
    /// `Focus Pane`, and their keys fold into one ribbon (`←↓↑→ Focus Pane`).
    pub hint_label: Option<&'static str>,
    /// Where the action's key sorts in its modifier group of the hint bar.
    pub hint_tier: HintTier,
}

/// Build one `core:` seed entry with `is_continuous` `false`, no hint label
/// and the [`HintTier::Other`] hint tier.
///
/// # Panics
/// Panics if `action_name` violates the action-name grammar.
fn build_core_action_seed(
    action_name: &'static str,
    display_name: &str,
    description: &str,
    scope: ActionScope,
    target_kinds: Vec<TargetKind>,
    handler: ActionHandlerReference,
) -> (ActionReference, ActionMetadata) {
    let action_reference = ActionReference::from_core_action_name(action_name)
        .expect("core seed action name must satisfy the action-name grammar");
    let action_metadata = ActionMetadata {
        display_name: display_name.to_string(),
        description: description.to_string(),
        scope,
        target_kinds,
        handler,
        is_continuous: false,
        hint_label: None,
        hint_tier: HintTier::Other,
    };
    (action_reference, action_metadata)
}

/// The hint-bar label for `core:mouse-select` while the mode is **off**, and
/// the action's registry display name.
pub const MOUSE_SELECT_HINT: &str = "Mouse Select";

/// The hint-bar label for `core:mouse-select` while the mode is **on**. A
/// frame painted while mouse-select is on shows it in place of
/// [`MOUSE_SELECT_HINT`].
pub const MOUSE_UNSELECT_HINT: &str = "Mouse Unselect";

/// The registry display name of `core:begin-tiled-pane-placement`, and the
/// hint-bar label of `core:confirm-pane-move-choice` while no floating pane
/// takes the viewer's input.
pub const MOVE_TILED_PANE_HINT: &str = "Move Tiled Pane";

/// The registry display name of `core:begin-floating-pane-move`, and the
/// hint-bar label of `core:confirm-pane-move-choice` while a floating pane
/// takes the viewer's input.
pub const MOVE_FLOATING_PANE_HINT: &str = "Move Floating Pane";

/// The registry display name of `core:confirm-pane-move-choice`. Every frame
/// shows [`MOVE_TILED_PANE_HINT`] or [`MOVE_FLOATING_PANE_HINT`] in its place.
pub const MOVE_FOCUSED_PANE_HINT: &str = "Move Focused Pane";

/// The built-in action table, loaded into the runtime registry at startup.
/// `koshi actions list` prints the entries in this order.
///
/// The `begin-pane-placement` action starts the viewer-local pane placement
/// mode, or the pane move chooser while the viewer draws a floating pane it
/// did not pin. The placement, chooser and floating pane move actions select
/// and confirm viewer-local targets. The floating pane tray actions open,
/// close and drive the viewer's tray.
/// The other command-backed actions use values their NAME bakes into the command
/// the resolver builds — `lock`/`unlock` both build `SetLockMode`; the
/// `new-pane-<direction>`, `focus-pane-<direction>`, and
/// `resize-pane-<direction>` families each build their family's command with
/// the named direction; `new-pane-stacked` and `new-pane-floating` build
/// `NewPane` with a stacked or a floating placement;
/// `focus-next-floating-pane` and `focus-previous-floating-pane` build
/// `FocusPane` with the floating pane target they name;
/// `next-tab`/`previous-tab`/`focus-tab` all build `FocusTab`; `undock-pane`
/// builds `PlacePane` with a floating target; `pin-pane`/`unpin-pane` both
/// build `SetPanePinned`; `toggle-pane-pinned` builds `TogglePanePinned`;
/// `minimize-pane` builds `SetPaneMinimized`; `minimize-all-floating-panes` and
/// `restore-all-floating-panes` both build `SetAllFloatingPanesMinimized`;
/// `toggle-minimize-all-floating-panes` builds
/// `ToggleAllFloatingPanesMinimized`.
///
/// The `resize-pane-*` and `focus-pane-*` direction families,
/// `focus-next-floating-pane`, `focus-previous-floating-pane`, and
/// `scroll-pane-up`/`scroll-pane-down` are `is_continuous`; every other
/// action is not, bare `resize-pane`, `focus-pane` and `scroll-pane`
/// included.
///
/// Each direction family carries one hint label: `new-pane-*` reads
/// `Split Pane`, `resize-pane-*` `Resize Pane`, `select-pane-target-*`
/// `Select Pane Target`, `select-pane-insertion-*` `Select Pane Insertion`,
/// `move-floating-pane-*` `Move Floating Pane`, `select-tray-tile-*`
/// `Select Tray Tile`, `focus-pane-*` `Focus Pane`,
/// `scroll-pane-up`/`scroll-pane-down` `Scroll Pane`, and
/// `select-newer-notification`/`select-older-notification`
/// `Select Notification`. Every other action has none.
///
/// Every `confirm-*` and `cancel-*` action, `new-pane`, `close-pane`,
/// `close-pane-tree`, `new-tab`, `close-tab`, `lock`, `unlock`,
/// `close-floating-pane-tray` and `close-notification-center` are
/// [`HintTier::Essential`]. The direction
/// families, `next-tab`, `previous-tab`, and the next and previous floating
/// pane and placement tab actions are [`HintTier::Navigation`]. Every other
/// action is [`HintTier::Other`].
#[must_use]
pub fn build_core_action_seeds() -> Vec<(ActionReference, ActionMetadata)> {
    use ActionHandlerReference::{CoreClient, CoreCommand};
    use ActionScope::{Client, PaneSession, Tab};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let mut action_seeds = vec![
        // --- Panes ---
        build_core_action_seed(
            "new-pane",
            "New Pane",
            "Split the focused pane and start a shell in the new one",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-left",
            "New Pane Left",
            "Split the focused pane and open the new one on the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-down",
            "New Pane Down",
            "Split the focused pane and open the new one below",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-up",
            "New Pane Up",
            "Split the focused pane and open the new one above",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-right",
            "New Pane Right",
            "Split the focused pane and open the new one on the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-stacked",
            "New Stacked Pane",
            "Add a new pane to the focused pane's stack, sharing its space",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "new-pane-floating",
            "New Floating Pane",
            "Open a new floating pane above the tiled panes and focus it on the issuing client's screen",
            PaneSession,
            vec![Session, ClientTarget],
            CoreCommand(CommandKind::NewPane),
        ),
        build_core_action_seed(
            "close-pane",
            "Close Pane",
            "Close the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
        ),
        build_core_action_seed(
            "close-pane-tree",
            "Close Pane Tree",
            "Close the focused pane and kill every process it started",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ClosePane),
        ),
        build_core_action_seed(
            "resize-pane",
            "Resize Pane",
            "Grow or shrink the focused pane along one edge",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-left",
            "Resize Pane Left",
            "Move the focused pane's border one cell to the left",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-down",
            "Resize Pane Down",
            "Move the focused pane's border one cell down",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-up",
            "Resize Pane Up",
            "Move the focused pane's border one cell up",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "resize-pane-right",
            "Resize Pane Right",
            "Move the focused pane's border one cell to the right",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::ResizePane),
        ),
        build_core_action_seed(
            "begin-pane-placement",
            "Begin Pane Placement",
            "Open pane placement mode for the focused pane",
            PaneSession,
            vec![Pane],
            CoreClient(ClientActionKind::BeginPanePlacement),
        ),
        build_core_action_seed(
            "begin-dock-undock",
            "Dock Or Undock Pane",
            "Open dock/undock mode for the pane that takes the input",
            PaneSession,
            vec![Pane],
            CoreClient(ClientActionKind::BeginDockUndock),
        ),
        build_core_action_seed(
            "select-pane-target-left",
            "Select Pane Target Left",
            "Select the visible pane target to the left in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Left)),
        ),
        build_core_action_seed(
            "select-pane-target-down",
            "Select Pane Target Down",
            "Select the visible pane target below in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Down)),
        ),
        build_core_action_seed(
            "select-pane-target-up",
            "Select Pane Target Up",
            "Select the visible pane target above in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Up)),
        ),
        build_core_action_seed(
            "select-pane-target-right",
            "Select Pane Target Right",
            "Select the visible pane target to the right in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneTarget(Direction::Right)),
        ),
        build_core_action_seed(
            "select-pane-insertion-left",
            "Select Pane Insertion Left",
            "Select the left insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Left)),
        ),
        build_core_action_seed(
            "select-pane-insertion-down",
            "Select Pane Insertion Down",
            "Select the lower insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Down)),
        ),
        build_core_action_seed(
            "select-pane-insertion-up",
            "Select Pane Insertion Up",
            "Select the upper insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Up)),
        ),
        build_core_action_seed(
            "select-pane-insertion-right",
            "Select Pane Insertion Right",
            "Select the right insertion edge in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPaneInsertion(Direction::Right)),
        ),
        build_core_action_seed(
            "cycle-pane-placement-span",
            "Cycle Pane Placement Span",
            "Cycle the available insertion spans in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CyclePanePlacementSpan),
        ),
        build_core_action_seed(
            "toggle-pane-placement-stack",
            "Toggle Pane Placement Stack",
            "Switch the selected target between beside a pane or stack and into that stack",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::TogglePanePlacementStack),
        ),
        build_core_action_seed(
            "select-next-placement-tab",
            "Select Next Placement Tab",
            "Preview the next visible tab in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectNextPlacementTab),
        ),
        build_core_action_seed(
            "select-previous-placement-tab",
            "Select Previous Placement Tab",
            "Preview the previous visible tab in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPreviousPlacementTab),
        ),
        build_core_action_seed(
            "confirm-pane-placement",
            "Confirm Pane Placement",
            "Submit the selected pane placement in placement mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ConfirmPanePlacement),
        ),
        build_core_action_seed(
            "cancel-pane-placement",
            "Cancel Pane Placement",
            "Discard the pane placement and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CancelPanePlacement),
        ),
        build_core_action_seed(
            "confirm-dock-undock",
            "Dock Or Undock",
            "Float the selected tiled pane where it is, or dock the selected floating pane at its target",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ConfirmDockUndock),
        ),
        build_core_action_seed(
            "cancel-dock-undock",
            "Cancel Dock Or Undock",
            "Leave dock/undock mode and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CancelDockUndock),
        ),
        build_core_action_seed(
            "begin-tiled-pane-placement",
            MOVE_TILED_PANE_HINT,
            "Open pane placement mode for the focused pane of the active tab",
            PaneSession,
            vec![Pane],
            CoreClient(ClientActionKind::BeginTiledPanePlacement),
        ),
        build_core_action_seed(
            "begin-floating-pane-move",
            MOVE_FLOATING_PANE_HINT,
            "Open floating pane move mode on a floating pane this client shows",
            PaneSession,
            vec![Pane],
            CoreClient(ClientActionKind::BeginFloatingPaneMove),
        ),
        build_core_action_seed(
            "confirm-pane-move-choice",
            MOVE_FOCUSED_PANE_HINT,
            "Open the move mode for the kind of the focused pane in the pane move chooser",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ConfirmPaneMoveChoice),
        ),
        build_core_action_seed(
            "cancel-pane-move-choice",
            "Cancel Move",
            "Close the pane move chooser and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CancelPaneMoveChoice),
        ),
        build_core_action_seed(
            "move-floating-pane-left",
            "Move Floating Pane Left",
            "Move the selected floating pane one cell left in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::MoveFloatingPane(Direction::Left)),
        ),
        build_core_action_seed(
            "move-floating-pane-down",
            "Move Floating Pane Down",
            "Move the selected floating pane one cell down in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::MoveFloatingPane(Direction::Down)),
        ),
        build_core_action_seed(
            "move-floating-pane-up",
            "Move Floating Pane Up",
            "Move the selected floating pane one cell up in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::MoveFloatingPane(Direction::Up)),
        ),
        build_core_action_seed(
            "move-floating-pane-right",
            "Move Floating Pane Right",
            "Move the selected floating pane one cell right in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::MoveFloatingPane(Direction::Right)),
        ),
        build_core_action_seed(
            "select-next-floating-pane",
            "Select Next Floating Pane",
            "Select and focus the next floating pane, in the order the panes became floating, in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectNextFloatingPane),
        ),
        build_core_action_seed(
            "select-previous-floating-pane",
            "Select Previous Floating Pane",
            "Select and focus the previous floating pane, in the order the panes became floating, in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectPreviousFloatingPane),
        ),
        build_core_action_seed(
            "confirm-floating-pane-move",
            "Confirm Floating Pane Move",
            "Move the selected floating pane to its draft in floating pane move mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ConfirmFloatingPaneMove),
        ),
        build_core_action_seed(
            "cancel-floating-pane-move",
            "Cancel Floating Pane Move",
            "Discard the floating pane draft and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CancelFloatingPaneMove),
        ),
        build_core_action_seed(
            "toggle-floating-pane-tray",
            "Toggle Floating Pane Tray",
            "Open the floating pane tray under the tabline's tray button, or close it when it is open",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ToggleFloatingPaneTray),
        ),
        build_core_action_seed(
            "close-floating-pane-tray",
            "Close Floating Pane Tray",
            "Close the floating pane tray and return to the base input mode",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CloseFloatingPaneTray),
        ),
        build_core_action_seed(
            "select-tray-tile-left",
            "Select Tray Tile Left",
            "Select the tile to the left in the floating pane tray",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectTrayTile(Direction::Left)),
        ),
        build_core_action_seed(
            "select-tray-tile-down",
            "Select Tray Tile Down",
            "Select the tile below in the floating pane tray",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectTrayTile(Direction::Down)),
        ),
        build_core_action_seed(
            "select-tray-tile-up",
            "Select Tray Tile Up",
            "Select the tile above in the floating pane tray",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectTrayTile(Direction::Up)),
        ),
        build_core_action_seed(
            "select-tray-tile-right",
            "Select Tray Tile Right",
            "Select the tile to the right in the floating pane tray",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectTrayTile(Direction::Right)),
        ),
        build_core_action_seed(
            "activate-selected-tray-tile",
            "Activate Selected Tray Tile",
            "Minimize or restore the selected tile's floating pane on this client's screen; on the + new tile, close the tray and create a floating pane",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ActivateSelectedTrayTile),
        ),
        build_core_action_seed(
            "toggle-notification-center",
            "Toggle Notification Center",
            "Open this client's notification center on the right side, or close it when it is open",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ToggleNotificationCenter),
        ),
        build_core_action_seed(
            "close-notification-center",
            "Close Notification Center",
            "Close the notification center and keep its entries",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::CloseNotificationCenter),
        ),
        build_core_action_seed(
            "clear-notifications",
            "Clear Notifications",
            "Remove every entry from this client's notification center and close it",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::ClearNotifications),
        ),
        build_core_action_seed(
            "select-newer-notification",
            "Select Newer Notification",
            "Select the notification card above the selected one in the notification center",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectNewerNotification),
        ),
        build_core_action_seed(
            "select-older-notification",
            "Select Older Notification",
            "Select the notification card below the selected one in the notification center",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::SelectOlderNotification),
        ),
        build_core_action_seed(
            "dismiss-selected-notification",
            "Dismiss Selected Notification",
            "Remove the selected notification card from this client's notification center",
            Client,
            vec![ClientTarget],
            CoreClient(ClientActionKind::DismissSelectedNotification),
        ),
        build_core_action_seed(
            "move-pane",
            "Move Pane",
            "Swap a pane with its visible neighbor in one direction and commit the swap at once",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::MovePane),
        ),
        build_core_action_seed(
            "place-pane",
            "Place Pane",
            "Put a pane into a tab's tiled layout, beside a pane or into its stack; a floating pane docks",
            PaneSession,
            vec![Pane, TabTarget],
            CoreCommand(CommandKind::PlacePane),
        ),
        build_core_action_seed(
            "undock-pane",
            "Undock Pane",
            "Float the focused tiled pane above the tiled panes, at the top-left cell and the size where it is drawn, each axis cut to 90% of the smallest pane area",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::PlacePane),
        ),
        build_core_action_seed(
            "dock-pane",
            "Dock Pane",
            "Insert a floating pane into a tab's tiled layout",
            PaneSession,
            vec![Pane, TabTarget],
            CoreCommand(CommandKind::PlacePane),
        ),
        build_core_action_seed(
            "move-floating-pane",
            "Move Floating Pane To Cell",
            "Move a floating pane to a top-left cell on one client's screen",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::MoveFloatingPane),
        ),
        build_core_action_seed(
            "pin-pane",
            "Pin Pane",
            "Pin the focused floating pane at its top-left cell on the issuing client's screen",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::SetPanePinned),
        ),
        build_core_action_seed(
            "unpin-pane",
            "Unpin Pane",
            "Unpin the focused floating pane on the issuing client's screen",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::SetPanePinned),
        ),
        build_core_action_seed(
            "toggle-pane-pinned",
            "Toggle Pane Pinned",
            "Pin the focused floating pane at its top-left cell on the issuing client's screen, or unpin it when it is pinned there",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::TogglePanePinned),
        ),
        build_core_action_seed(
            "minimize-pane",
            "Minimize Pane",
            "Minimize the focused floating pane on the issuing client's screen",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::SetPaneMinimized),
        ),
        build_core_action_seed(
            "restore-pane",
            "Restore Pane",
            "Restore a minimized floating pane on one client's screen and focus it",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::SetPaneMinimized),
        ),
        build_core_action_seed(
            "minimize-all-floating-panes",
            "Minimize All Floating Panes",
            "Minimize every floating pane the issuing client shows",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetAllFloatingPanesMinimized),
        ),
        build_core_action_seed(
            "restore-all-floating-panes",
            "Restore All Floating Panes",
            "Restore every floating pane the issuing client minimized, and focus the last one",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetAllFloatingPanesMinimized),
        ),
        build_core_action_seed(
            "toggle-minimize-all-floating-panes",
            "Toggle Minimize All Floating Panes",
            "Minimize every floating pane the issuing client shows, or restore every one when it shows none",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleAllFloatingPanesMinimized),
        ),
        build_core_action_seed(
            "focus-pane",
            "Focus Pane",
            "Move the issuing client's focus to a pane",
            Client,
            vec![Pane, ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-left",
            "Focus Pane Left",
            "Move the issuing client's focus to the pane on the left",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-down",
            "Focus Pane Down",
            "Move the issuing client's focus to the pane below",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-up",
            "Focus Pane Up",
            "Move the issuing client's focus to the pane above",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-pane-right",
            "Focus Pane Right",
            "Move the issuing client's focus to the pane on the right",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-next-floating-pane",
            "Focus Next Floating Pane",
            "Show and focus the floating pane after the focused one, in the order the panes became floating",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "focus-previous-floating-pane",
            "Focus Previous Floating Pane",
            "Show and focus the floating pane before the focused one, in the order the panes became floating",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusPane),
        ),
        build_core_action_seed(
            "scroll-pane",
            "Scroll Pane",
            "Scroll a pane's view by a chosen number of lines",
            Client,
            vec![ClientTarget, Pane],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "scroll-pane-up",
            "Scroll Pane Up",
            "Scroll the focused pane's view toward its history",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "scroll-pane-down",
            "Scroll Pane Down",
            "Scroll the focused pane's view toward live output",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ScrollPane),
        ),
        build_core_action_seed(
            "toggle-pane-fullscreen",
            "Toggle Pane Fullscreen",
            "Toggle fullscreen for the focused pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::TogglePaneFullscreen),
        ),
        build_core_action_seed(
            "write-to-pane",
            "Write To Pane",
            "Send text to a pane's shell, as if it had been typed there",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::WriteToPane),
        ),
        // --- Tabs ---
        build_core_action_seed(
            "new-tab",
            "New Tab",
            "Create a new tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::NewTab),
        ),
        build_core_action_seed(
            "close-tab",
            "Close Tab",
            "Close the focused tab",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::CloseTab),
        ),
        build_core_action_seed(
            "focus-tab",
            "Focus Tab",
            "Switch the issuing client's view to a specific tab",
            Client,
            vec![TabTarget, ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "next-tab",
            "Next Tab",
            "Switch the issuing client's view to the next tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "previous-tab",
            "Previous Tab",
            "Switch the issuing client's view to the previous tab",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::FocusTab),
        ),
        build_core_action_seed(
            "move-tab",
            "Move Tab",
            "Move the focused tab to a new index",
            Tab,
            vec![TabTarget],
            CoreCommand(CommandKind::MoveTab),
        ),
        // --- Session ---
        build_core_action_seed(
            "quit",
            "Quit",
            "Leave the session, ending it when auto-close-session is on and no other client stays",
            Client,
            vec![ClientTarget, Session],
            CoreCommand(CommandKind::Quit),
        ),
        // --- Lock mode ---
        build_core_action_seed(
            "toggle-lock",
            "Toggle Lock",
            "Toggle pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleLockMode),
        ),
        build_core_action_seed(
            "lock",
            "Lock",
            "Enable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
        ),
        build_core_action_seed(
            "unlock",
            "Unlock",
            "Disable pass-through lock mode for the issuing client",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::SetLockMode),
        ),
        // --- Mouse select ---
        build_core_action_seed(
            "mouse-select",
            MOUSE_SELECT_HINT,
            "Toggle grabbing the mouse for text selection, so a drag highlights \
             in koshi even over a program that asked for the mouse",
            Client,
            vec![ClientTarget],
            CoreCommand(CommandKind::ToggleMouseSelect),
        ),
        // --- Run ---
        build_core_action_seed(
            "run",
            "Run Command",
            "Spawn a command in a new pane",
            PaneSession,
            vec![Pane],
            CoreCommand(CommandKind::NewPane),
        ),
    ];

    // Repeat-from-prefix actions: fired from a multi-chord binding, the
    // prefix stays armed and the next chord alone fires again
    // (`<C-s> ← ← ←`, `<C-p> ← ← ←`).
    for (action_reference, action_metadata) in &mut action_seeds {
        if matches!(
            action_reference.action_name.get_name(),
            "resize-pane-left"
                | "resize-pane-down"
                | "resize-pane-up"
                | "resize-pane-right"
                | "focus-pane-left"
                | "focus-pane-down"
                | "focus-pane-up"
                | "focus-pane-right"
                | "focus-next-floating-pane"
                | "focus-previous-floating-pane"
                | "scroll-pane-up"
                | "scroll-pane-down"
        ) {
            action_metadata.is_continuous = true;
        }
    }

    for (action_reference, action_metadata) in &mut action_seeds {
        let action_name = action_reference.action_name.get_name();
        action_metadata.hint_label = match action_name {
            "new-pane-left" | "new-pane-down" | "new-pane-up" | "new-pane-right" => {
                Some("Split Pane")
            }
            "resize-pane-left" | "resize-pane-down" | "resize-pane-up" | "resize-pane-right" => {
                Some("Resize Pane")
            }
            "select-pane-target-left"
            | "select-pane-target-down"
            | "select-pane-target-up"
            | "select-pane-target-right" => Some("Select Pane Target"),
            "select-pane-insertion-left"
            | "select-pane-insertion-down"
            | "select-pane-insertion-up"
            | "select-pane-insertion-right" => Some("Select Pane Insertion"),
            "move-floating-pane-left"
            | "move-floating-pane-down"
            | "move-floating-pane-up"
            | "move-floating-pane-right" => Some("Move Floating Pane"),
            "select-tray-tile-left"
            | "select-tray-tile-down"
            | "select-tray-tile-up"
            | "select-tray-tile-right" => Some("Select Tray Tile"),
            "focus-pane-left" | "focus-pane-down" | "focus-pane-up" | "focus-pane-right" => {
                Some("Focus Pane")
            }
            "scroll-pane-up" | "scroll-pane-down" => Some("Scroll Pane"),
            "select-newer-notification" | "select-older-notification" => {
                Some("Select Notification")
            }
            _ => None,
        };
        let is_direction_family_member = action_metadata.hint_label.is_some();
        action_metadata.hint_tier = match action_name {
            "new-pane"
            | "close-pane"
            | "close-pane-tree"
            | "new-tab"
            | "close-tab"
            | "lock"
            | "unlock"
            | "close-floating-pane-tray"
            | "close-notification-center" => HintTier::Essential,
            _ if action_name.starts_with("confirm-") || action_name.starts_with("cancel-") => {
                HintTier::Essential
            }
            "next-tab"
            | "previous-tab"
            | "focus-next-floating-pane"
            | "focus-previous-floating-pane"
            | "select-next-floating-pane"
            | "select-previous-floating-pane"
            | "select-next-placement-tab"
            | "select-previous-placement-tab" => HintTier::Navigation,
            _ if is_direction_family_member => HintTier::Navigation,
            _ => HintTier::Other,
        };
    }

    action_seeds
}

#[cfg(test)]
mod tests;
