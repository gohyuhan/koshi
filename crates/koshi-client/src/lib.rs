//! The viewer half of koshi: one attached terminal's own side of a session.
//!
//! A session is authoritative over tabs, panes, and the processes inside them.
//! A viewer owns the terminal in front of the user: its size, the settings it
//! reads from its own config, the colors it paints koshi's chrome with, and the
//! keymap it resolves its own keys against. It also decides what every key and
//! mouse event over its frame means. The two talk only through the session's
//! command door and its event feed.
//!
//! Colors live with the viewer: the frame a session hands out says *which pane
//! is focused*, and each viewer looks up what "focused" looks like in its own
//! theme. Two viewers of one session can paint it two different ways at once.

/// The bare `koshi` launch.
pub mod app;

/// The attached client: join a running session over its control socket and
/// read its event stream. A switch re-attaches the same terminal to the named
/// session; a broken link to a session on a server dials that server again for
/// up to 120 seconds while `remote-reconnect` is on; a detach, the session
/// ending, and a broken link to a session on this machine end the client.
pub mod attach;

mod floating_pane_tray;

pub mod input;

pub mod mouse;

/// The outer terminal an attached client owns: its viewer, its input thread,
/// and painting frames into it.
pub(crate) mod terminal;

pub mod theme;

use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_config::conflict::{
    build_keymap_layers, detect_conflicts, ConflictReport, KeymapVerdict,
};
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::layer::{
    ConfigLayers, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::types::ClientConfig;
use koshi_core::command::{Command, TiledPlacementTarget};
use koshi_core::geometry::Direction;
use koshi_core::key::PendingKeySequence;
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_core::{
    geometry::{PaneArea, Point, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
};
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_layout::placement::PlacedTabLayout;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::placement::{PlacedPreview, PlacementPreviewError};
use koshi_renderer::region::solve_core_regions;
use koshi_renderer::snapshot::{
    ClientSnapshot, FloatingPaneTray, LinkStatus, PlacementHandleHover, PlacementPreview,
    PlacementSnapshot, RenderSnapshot, SessionSnapshot,
};
use koshi_renderer::theme::Theme;

use crate::input::InputReadClock;
use crate::mouse::{LastPress, LeftButtonGesture, MouseCapture, SentFloatingPaneMove, TablinePeek};

#[cfg(test)]
mod tests;

/// Compute the pane area left by the compiled-in navigator and hint regions.
///
/// An `80x24` viewport reports `Reported(80x22)`. A viewport shorter than the
/// two rows reports zero rows instead of an invalid negative size.
#[must_use]
pub(crate) fn compute_core_pane_area(viewport_size: Size) -> PaneArea {
    PaneArea::Reported(solve_core_regions(viewport_size).pane_rect.size)
}

/// How long the pointer must remain over a placement tab before it is previewed.
pub(crate) const PLACEMENT_TAB_HOVER_DELAY_DURATION: Duration = Duration::from_millis(200);

/// The viewer-owned state for one placement interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementMode {
    /// The pane being placed. A left press on another pane in placement mode
    /// selects that pane instead.
    pub(crate) source_pane_id: PaneId,
    /// The tab whose layout holds the source pane, or `None` while the source
    /// pane floats. A painted frame that focuses a tiled source pane sets it to
    /// that frame's active tab, and each accepted preview sets it to the tab
    /// that preview names, or to `None` for a floating source: after
    /// `pane-123` moves from tab `#1` to tab `#2`, both set `#2`.
    pub(crate) source_tab_id: Option<TabId>,
    /// The tab currently being previewed.
    pub(crate) destination_tab_id: TabId,
    /// The edge used by the last insertion selection.
    pub(crate) placement_direction: Direction,
    /// The checked target selected from the held placement preview.
    pub(crate) placement_target: Option<TiledPlacementTarget>,
    /// The id of the placement Enter or a mouse drop confirmed for
    /// `placement_target`, until the session rejects it or a frame names it as
    /// the client's last applied placement. `None` before a confirmation.
    pub(crate) pending_placement_command_id: Option<CommandId>,
}

/// One viewer-owned mouse drag that may confirm a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementDrag {
    /// The screen position where the drag began.
    pub(crate) start_position: Point,
    /// Whether the pointer has reached a different position during this drag.
    pub(crate) has_pointer_moved: bool,
    /// Shift+drag selects insertion targets; a plain drag selects swaps.
    pub(crate) is_insertion_drag: bool,
}

/// A placement tab hover waiting to select its tab as the destination once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementTabHover {
    /// The tab under the pointer.
    pub(crate) tab_id: TabId,
    /// When the pointer entered the tab.
    pub(crate) entered_at: Instant,
    /// Whether the hover already selected its tab as the destination.
    pub(crate) is_destination_tab_selected: bool,
}

/// How long the current placement mode lasts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PlacementModeLifetime {
    /// Until Esc. `core:begin-pane-placement` (`<C-p> m` by default) opens
    /// this lifetime. With `stay-in-pane-placement-mode-after-placement`
    /// `#false`, an accepted placement also ends it.
    #[default]
    UntilCancelled,
    /// Until the pane-handle drag that opened it ends. After a release that
    /// confirmed a placement, it lasts until a frame shows that placement,
    /// the session rejects it, or the first frame after a resync arrives,
    /// whatever `stay-in-pane-placement-mode-after-placement` says.
    UntilDragEnds,
}

/// One viewer's pane placement mode and the placement preview it holds. A
/// clone shares the held preview.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlacementState {
    /// The preview of placement mode's source pane and destination tab, from
    /// the newest applied frame whose preview names that pane and tab. `None`
    /// with no placement mode, and while no frame carried that preview yet.
    placement_preview: Option<Arc<PlacementPreview>>,
    /// The local placement interaction, if the viewer is choosing a destination.
    placement_mode: Option<PlacementMode>,
    /// How long the current placement interaction lasts.
    placement_mode_lifetime: PlacementModeLifetime,
    /// Whether [`Client::set_frame_view`] reconciles a submitted placement. It
    /// is set when a frame shows this viewer's applied placement, and when a
    /// resync, restart, or redial lost the session's answer. The reconciling
    /// view clears the placement draft, and its tab or focus change does not
    /// cancel pane placement mode.
    needs_placement_reconciliation: bool,
    /// The mouse drag that placement mode currently owns.
    placement_drag: Option<PlacementDrag>,
    /// The tab hover waiting to select its tab as the destination.
    placement_tab_hover: Option<PlacementTabHover>,
    /// The session's answer to placement mode's selected target in the held
    /// available preview, as [`Client::update_placement_target_outcome`] last
    /// built it.
    placement_target_outcome: Option<PlacementTargetOutcome>,
}

/// The session's answer to placement mode's selected target in one available
/// placement preview, built once for that preview and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementTargetOutcome {
    /// The available preview the answer is built from.
    pub(crate) placement_preview: Arc<PlacementPreview>,
    /// The selected target.
    pub(crate) placement_target: TiledPlacementTarget,
    /// The preview's displayed tab with the target placed, and the layout a
    /// placement command carries for it, or the session's refusal of the
    /// target ([`PlacementSnapshot::build_placed_preview`]).
    pub(crate) placed_preview: Result<PlacedPreview, PlacementPreviewError>,
}

/// The viewer-local mode that moves a pane without pane placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneMoveMode {
    /// The move chooser, which the `pane-move-choice` keymap mode resolves
    /// keys in and the tabline tags `MOVE`. `core:begin-pane-placement`
    /// (`<C-p> m` by default) opens it while this viewer draws an unpinned
    /// floating pane.
    Choice,
    /// Floating pane move mode, which the `floating-pane-move` keymap mode
    /// resolves keys in and the tabline tags `MOVE FLOAT`. It owns the mouse.
    FloatingPaneMove(FloatingPaneMoveSelection),
}

/// The floating pane that floating pane move mode moves, the cell its arrow
/// keys moved it to, and whether the mode waits for the move it sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingPaneMoveSelection {
    /// The floating pane that the arrow keys, Enter, and a mouse drag move.
    pub(crate) selected_pane_id: PaneId,
    /// The top-left cell the arrow keys moved the selected pane to, counted
    /// from the pane-area origin, which this viewer draws until Enter sends
    /// it. `None` before an arrow key moves the pane, after Enter, and after a
    /// painted frame that does not draw the pane or draws it pinned.
    pub(crate) draft_top_left_cell: Option<Point>,
    /// The command id of the floating pane move that the mode sent from Enter
    /// or a mouse release, while the mode waits for the session's answer.
    /// `None` while the mode waits for no move. The wait ends when the session
    /// rejects the move, or at the first frame applied with no sent move drawn
    /// ahead of the session: the move ran when that frame names it. While it
    /// is set, the mode stays on, keys that move, select, open, or close
    /// change nothing, and a left press selects and drags nothing. This holds
    /// also after a resync or a new link drops the sent move that this viewer
    /// draws ahead of the session.
    pub(crate) pending_move_command_id: Option<CommandId>,
}

/// Dock/undock mode, which the `dock-undock` keymap mode resolves keys in and
/// the tabline tags `DOCK`. The floating pane the mode selected is the source
/// pane of pane placement mode, which picks where that pane docks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DockUndockMode {
    /// The tiled pane the mode selected, or `None` while the mode selected a
    /// floating pane or nothing.
    pub(crate) tiled_pane_selection: Option<TiledPaneSelection>,
}

/// The tiled pane that dock/undock mode selected, and the undock the mode
/// sent for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TiledPaneSelection {
    /// The tiled pane that Enter, or a drag that starts on it, floats where
    /// this viewer draws it.
    pub(crate) pane_id: PaneId,
    /// The id of the `PlacePane` that floats `pane_id`, while the mode waits
    /// for the session's answer. `None` while the mode waits for no undock.
    pub(crate) pending_undock_command_id: Option<CommandId>,
}

/// The viewer fields [`Client::apply_render_snapshot`] changes, copied before
/// one frame is applied. [`Client::restore_viewer`] puts them back.
pub(crate) struct ViewerRestorePoint {
    /// The base input mode.
    lock_mode: LockMode,
    /// The lock command whose lock mode the viewer shows before the session
    /// runs it.
    pending_lock_command_id: Option<CommandId>,
    /// Whether mouse-select is on.
    is_mouse_selection_enabled: bool,
    /// The multi-chord binding being typed.
    pending_key_sequence: Option<PendingKeySequence>,
    /// Where the tab strip is scrolled.
    tabline_peek: Option<TablinePeek>,
    /// The active tab of the newest painted frame.
    active_tab_id: Option<TabId>,
    /// The focused pane of the newest painted frame.
    focused_pane_id: Option<PaneId>,
    /// The focused pane in the active tab of the newest painted frame.
    active_tab_focused_pane_id: Option<PaneId>,
    /// The tab ids of the newest painted frame.
    visible_tab_ids: Vec<TabId>,
    /// Pane placement mode and its held preview.
    placement_state: PlacementState,
    /// The move chooser or floating pane move mode.
    pane_move_mode: Option<PaneMoveMode>,
    /// The floating pane move a grab-handle release sent, which this viewer
    /// draws ahead of the session.
    sent_floating_pane_move: Option<SentFloatingPaneMove>,
    /// Dock/undock mode and the tiled pane it selected.
    dock_undock_mode: Option<DockUndockMode>,
    /// The open floating pane tray.
    floating_pane_tray: Option<FloatingPaneTray>,
}

/// What a viewer-local action, or a mouse event that pane placement owns, asks
/// of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlacementInputAction {
    /// Focus the pane a mouse pickup selected as the placement source pane.
    FocusPane {
        /// The pane the pickup selected.
        pane_id: PaneId,
    },
    /// Send `command` under `command_id`, issued by this viewer's key
    /// binding or by a mouse event in dock/undock mode: the `PlacePane` that
    /// floats the tiled pane dock/undock mode selected, the `MoveFloatingPane`
    /// that floating pane move mode or dock/undock mode recorded as sent, the
    /// `FocusPane` that focuses the floating pane floating pane move mode
    /// selected, or the `SetPaneMinimized` that toggles the floating pane tray's
    /// selected tile.
    SubmitCommand {
        /// The id the command carries.
        command_id: CommandId,
        /// The command to send.
        command: Command,
    },
    /// Confirm the tiled placement that placement mode recorded as pending
    /// under `command_id` (`ConfirmPanePlacement`).
    ConfirmPlacement {
        /// The id the confirmation carries.
        command_id: CommandId,
        /// The pane being placed.
        source_pane_id: PaneId,
        /// Where the pane goes.
        placement_target: TiledPlacementTarget,
        /// The destination tab as this viewer draws it after the placement
        /// ([`PlacementSnapshot::build_placed_preview`]).
        expected_placed_tab_layout: Box<PlacedTabLayout>,
    },
    /// Consume the event without a request or command.
    Consumed,
}

/// One attached terminal's view side: its id, its own terminal size, its event
/// feed from the session, the settings it read from its own config, the chrome
/// colors and keymap resolved from them, and the outer-terminal restore guard.
///
/// The binary's event loop drives it; it can never mutate session or pane
/// data.
pub struct Client {
    /// This client's identifier, the one its input events and commands carry.
    client_id: ClientId,
    /// The session that supplied the current event stream.
    session_id: Option<SessionId>,
    /// The client's own outer-terminal size in cells. Updated from resize
    /// events and reported to the session, which reconciles tab sizes from
    /// every viewer's report; this copy is the client's alone.
    viewport_size: Size,
    /// This viewer's stored config overrides, one layer per config file, as
    /// [`load_startup_config`](Self::load_startup_config) last left them. A
    /// refused `keybinding.kdl` leaves its layer empty.
    config_layers: ConfigLayers,
    /// The settings this viewer owns, folded from [`config_layers`](Self::config_layers).
    client_config: ClientConfig,
    /// The chrome colors [`client_config`](Self::client_config)'s theme
    /// resolves to, held resolved: a frame reads them by borrow.
    theme: Theme,
    /// The keymap this viewer resolves its own keys against, built from
    /// [`client_config`](Self::client_config)'s keybindings and the action
    /// table.
    keymap_catalog: KeymapHintCatalog,
    /// The action table a bound name is checked against — for the hint bar's
    /// labels and the `is_continuous` flag a repeat-capable binding re-arms on.
    /// Dispatch itself happens on the session, against its own table.
    registry: ActionRegistry,
    /// This viewer's base input mode. It decides what a key means when no local
    /// submode owns the keyboard. Pane placement mode, the move chooser,
    /// floating pane move mode, dock/undock mode, and the floating pane tray
    /// overlay this value without changing it: each returns to Normal or
    /// Locked.
    /// The session keeps its own copy, which `koshi lock --client` reaches and
    /// `koshi list-clients` reports.
    lock_mode: LockMode,
    /// The newest lock command this viewer sent, or holds to send, whose lock
    /// mode [`lock_mode`](Self::lock_mode) shows before the session runs it.
    /// While it is set, a painted frame that does not name it as the last
    /// applied lock command leaves `lock_mode` as it is. `None` when no lock
    /// command waits.
    pending_lock_command_id: Option<CommandId>,
    /// Whether this viewer grabs the mouse for text selection. It decides what
    /// a press means before anything is sent. The session keeps its own copy,
    /// which the frame carries for the mode indicator and the hint bar's label.
    is_mouse_selection_enabled: bool,
    /// The multi-chord binding being typed, if any. Held chords belong to
    /// koshi and never reach a pane.
    pending_key_sequence: Option<PendingKeySequence>,
    /// The read times of this terminal's input events, shared with its input
    /// thread: no input timer is judged at the clock while an event the thread
    /// read is still on its way.
    input_read_clock: Arc<InputReadClock>,
    /// The most recent mouse press, compared with the next press to tell a
    /// double click from two separate clicks. `None` before this viewer has
    /// pressed anything.
    last_mouse_press: Option<LastPress>,
    /// The pane a forwarded press captured, the button that pressed it, and
    /// the cell its last forwarded report named. While a button is held, its
    /// drags and its release go to this pane even as the pointer leaves it,
    /// and a drag or release with no capture is not forwarded. Set when this
    /// viewer forwards a press, and cleared by the next release, which is
    /// forwarded whatever the frame says the pane's program reports. The events
    /// that `unreleased_mouse_capture` lists end it before that. A new link to
    /// the session drops it with no release owed
    /// ([`end_mouse_gestures`](Client::end_mouse_gestures)).
    ///
    /// A press always names its button; some terminals report every drag and
    /// release as the left button. Drags and the release read the stored
    /// button.
    mouse_capture: Option<MouseCapture>,
    /// A capture this viewer ended before its button's release reached the
    /// pane's program: a press of the same button, a forwarded press of
    /// another button, placement mode opening, or a painted frame that no
    /// longer draws the pane ended it. The program is owed one release of
    /// that button at the capture's last cell
    /// ([`take_mouse_capture_release`](Client::take_mouse_capture_release)).
    /// A new link to the session drops it.
    unreleased_mouse_capture: Option<MouseCapture>,
    /// The one koshi gesture the held left button drives. The left press that
    /// begins it sets it. Its release, the next left press, and every event
    /// that ends held gestures clear it.
    left_button_gesture: Option<LeftButtonGesture>,
    /// The floating pane move a grab-handle release sent, drawn ahead of the
    /// session until an applied frame names it, the session rejects it, or
    /// the session drops events.
    sent_floating_pane_move: Option<SentFloatingPaneMove>,
    /// Where this viewer's tab strip is scrolled, and the tab it was scrolled
    /// on. `None` follows the active tab. A [`TablinePeek`] records the active
    /// tab and the first visible tab index. The peek belongs to the tab it was
    /// made on, and [`Client::note_active_tab`] throws it away as soon as the
    /// viewer sees a frame on another tab.
    tabline_peek: Option<TablinePeek>,
    /// The line the pane's view showed on its top row when the last edge-scroll
    /// step was asked for, awaiting the session's report of where the view
    /// landed. Set only for a scroll the selection drag's timer asked for.
    selection_scroll_origin_row_index: Option<u64>,
    /// The pane this viewer's pointer is over, or `None` when it is over chrome
    /// or off every pane. The renderer draws an unfocused hovered pane in the
    /// hover color.
    hovered_pane_id: Option<PaneId>,
    /// Where this viewer's pointer rests on a pane's placement handle or on
    /// the tiled top border that shows it, or `None` off both.
    placement_handle_hover: Option<PlacementHandleHover>,
    /// Whether this viewer holds a link to its session, and how it is getting
    /// one back when it does not. The tabline draws
    /// `RECONNECTING (attempt 4, retry in 8s)` while it holds
    /// `LinkStatus::Redialing(Reconnecting { attempt: 4, retry_in_seconds: Some(8) })`.
    link_status: LinkStatus,
    /// This viewer's pane placement mode and the placement preview it holds.
    placement_state: PlacementState,
    /// The active tab in the newest painted frame.
    active_tab_id: Option<TabId>,
    /// The focused pane in the newest painted frame.
    focused_pane_id: Option<PaneId>,
    /// The focused pane in the active tab of the newest painted frame, also
    /// while a floating pane takes this viewer's input. Pane placement mode
    /// that a key opens places this pane.
    active_tab_focused_pane_id: Option<PaneId>,
    /// The tab ids in display order in the newest painted frame.
    visible_tab_ids: Vec<TabId>,
    /// The move chooser or floating pane move mode, or `None` when neither is
    /// on. It is `None` while pane placement mode is on.
    pane_move_mode: Option<PaneMoveMode>,
    /// Dock/undock mode, or `None` when it is off. It is `None` while the move
    /// chooser or floating pane move mode is on.
    dock_undock_mode: Option<DockUndockMode>,
    /// The floating pane tray this viewer shows under its tabline button, or
    /// `None` while the tray is closed. It is `None` while any other local
    /// submode is on, and while the viewer is not [`LinkStatus::Linked`].
    floating_pane_tray: Option<FloatingPaneTray>,
    /// Restores the outer terminal when the client ends or the process
    /// panics. Held to be dropped with the client; nothing reads it.
    _terminal_cleanup_guard: TerminalCleanupGuard,
}

impl Client {
    /// Build a client from its id, its terminal's current size, and the
    /// outer-terminal cleanup guard.
    ///
    /// It starts on the built-in defaults: no stored config layers, the stock
    /// palette, and the shipped keymap. The files the user wrote arrive
    /// through [`load_startup_config`](Self::load_startup_config).
    #[must_use]
    pub fn from_client_id_and_viewport_size(
        client_id: ClientId,
        viewport_size: Size,
        terminal_cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        let config_layers = ConfigLayers::default();
        let client_config = config_layers.resolve_effective_client_config();
        let theme = theme::resolve_theme(&client_config.theme);
        let registry = ActionRegistry::new();
        let keymap_catalog = KeymapHintCatalog::from_registry(&registry);
        Client {
            client_id,
            session_id: None,
            viewport_size,
            config_layers,
            client_config,
            theme,
            keymap_catalog,
            registry,
            lock_mode: LockMode::Normal,
            pending_lock_command_id: None,
            is_mouse_selection_enabled: false,
            pending_key_sequence: None,
            input_read_clock: Arc::default(),
            last_mouse_press: None,
            mouse_capture: None,
            unreleased_mouse_capture: None,
            left_button_gesture: None,
            sent_floating_pane_move: None,
            tabline_peek: None,
            selection_scroll_origin_row_index: None,
            hovered_pane_id: None,
            placement_handle_hover: None,
            link_status: LinkStatus::Linked,
            placement_state: PlacementState::default(),
            active_tab_id: None,
            focused_pane_id: None,
            active_tab_focused_pane_id: None,
            visible_tab_ids: Vec::new(),
            pane_move_mode: None,
            dock_undock_mode: None,
            floating_pane_tray: None,
            _terminal_cleanup_guard: terminal_cleanup_guard,
        }
    }

    /// Apply the config files this viewer read at startup: `koshi.kdl`'s
    /// viewer-owned sections, the color theme, and `keybinding.kdl`. Each is
    /// `None` when its file is absent or failed to load, and its defaults then
    /// stand — a `None` `keybindings_config_layer` puts the keymap back on the
    /// built-ins and drops any sequence being typed.
    ///
    /// App settings and colors are typed values and always apply. Keybindings
    /// are all-or-nothing: conflict detection runs over the candidate against
    /// this viewer's action table, and it commits only on a
    /// [`KeymapVerdict::Apply`] — storing the layer, rebuilding the keymap,
    /// and dropping any sequence being typed. A [`KeymapVerdict::Reject`]
    /// keeps the running keymap, and puts the stored layer and the folded
    /// keybinding settings back on the built-ins.
    ///
    /// Returns the conflict report when `keybindings_config_layer` is `Some`,
    /// and `None` when it is `None`.
    pub fn load_startup_config(
        &mut self,
        app_config_layer: Option<PartialKoshiConfig>,
        theme_config_layer: Option<PartialThemeConfig>,
        keybindings_config_layer: Option<PartialKeybindingsConfig>,
    ) -> Option<ConflictReport> {
        self.config_layers = ConfigLayers::from_config_file_layers(
            app_config_layer.clone(),
            theme_config_layer.clone(),
            None,
        );
        self.client_config = self.config_layers.resolve_effective_client_config();
        self.theme = theme::resolve_theme(&self.client_config.theme);

        let Some(keybindings_config_layer) = keybindings_config_layer else {
            self.keymap_catalog = KeymapHintCatalog::from_registry(&self.registry);
            self.pending_key_sequence = None;
            return None;
        };
        let user_mode_bindings_by_name = keybindings_config_layer.mode_bindings_by_name.clone();
        let tentative_config_layers = ConfigLayers::from_config_file_layers(
            app_config_layer,
            theme_config_layer,
            Some(keybindings_config_layer),
        );
        let tentative_client_config = tentative_config_layers.resolve_effective_client_config();
        let keymap_layers = build_keymap_layers(
            user_mode_bindings_by_name,
            tentative_client_config.keybindings.leader,
        );
        let conflict_report = detect_conflicts(
            &keymap_layers,
            tentative_client_config.keybindings.leader,
            tentative_client_config.keybindings.unlock_alternative,
            tentative_client_config.keybindings.maximum_chord_depth,
            &self.registry,
        );
        if conflict_report.get_verdict() != KeymapVerdict::Apply {
            return Some(conflict_report);
        }
        self.config_layers = tentative_config_layers;
        self.client_config = tentative_client_config;
        self.keymap_catalog = KeymapHintCatalog::from_keymap_layers_config_and_registry(
            &keymap_layers,
            &self.client_config.keybindings,
            &self.registry,
        );
        // The new keymap drops the sequence being typed: it resolves to
        // nothing.
        self.pending_key_sequence = None;
        Some(conflict_report)
    }

    /// This client's identifier.
    #[must_use]
    pub fn get_client_id(&self) -> ClientId {
        self.client_id
    }

    /// Record `client_id`, the identifier the session minted for this viewer's
    /// current attach.
    /// Every command this viewer submits afterwards carries it.
    pub fn set_client_id(&mut self, client_id: ClientId) {
        if self.client_id != client_id && !self.is_placement_confirmation_pending() {
            self.clear_placement_draft();
        }
        self.client_id = client_id;
    }

    /// Record whether this viewer holds a link to its session. The tabline
    /// draws `RECONNECTING (attempt 4, retry in 8s)` for
    /// `LinkStatus::Redialing(Reconnecting { attempt: 4, retry_in_seconds: Some(8) })`.
    ///
    /// Dock/undock mode, pane placement mode, the move chooser, and floating
    /// pane move mode own the keyboard only while the viewer is
    /// [`LinkStatus::Linked`]. A status other than `Linked` closes the
    /// floating pane tray. A change that moves the keyboard between one of
    /// their keymaps and the base keymap drops an open key sequence, and no
    /// pane receives its chords.
    pub(crate) fn set_link_status(&mut self, link_status: LinkStatus) {
        let previous_active_input_mode = self.get_active_input_mode();
        self.link_status = link_status;
        if link_status != LinkStatus::Linked {
            self.floating_pane_tray = None;
        }
        if self.get_active_input_mode() != previous_active_input_mode {
            self.pending_key_sequence = None;
        }
    }

    /// The client's own outer-terminal size in cells.
    #[must_use]
    pub fn get_viewport_size(&self) -> Size {
        self.viewport_size
    }

    /// Record the outer terminal's new size. The caller also reports the
    /// resize to the session, which owns the reconciled tab sizes.
    pub fn set_viewport_size(&mut self, viewport_size: Size) {
        self.viewport_size = viewport_size;
    }

    /// Record the session that supplies the current event stream. Another
    /// session than the one recorded drops the unsent placement draft and
    /// closes the floating pane tray.
    pub(crate) fn set_session_id(&mut self, session_id: SessionId) {
        if self.session_id != Some(session_id) {
            self.clear_placement_draft();
            self.close_floating_pane_tray();
        }
        self.session_id = Some(session_id);
    }

    /// Apply the viewer state `render_snapshot` carries, in this order:
    /// whether the snapshot settles this viewer's placement confirmation by
    /// naming it (`settle_placement_command`), whether it settles a sent
    /// floating pane move by naming it (`settle_floating_pane_move`), the
    /// active tab with its focused pane and tab list, the lock mode, the
    /// tab-strip peek, whether mouse-select is on, where the tiled pane that
    /// dock/undock mode selected is drawn (`update_dock_undock_selection`), and
    /// the placement preview (`update_placement_preview`). A tab-strip peek
    /// made on another tab is dropped. The settled placement, the active tab,
    /// the lock mode, mouse-select, and an unavailable preview can end pane
    /// placement mode or clear its target. The settled move can end floating
    /// pane move mode, and a lock mode or mouse-select change ends the move
    /// chooser, floating pane move mode that waits for no move, and dock/undock
    /// mode that waits for no undock or dock, as `settle_placement_command`,
    /// `settle_floating_pane_move`, `set_frame_view`,
    /// [`set_lock_mode`](Self::set_lock_mode),
    /// [`set_mouse_selection_enabled`](Self::set_mouse_selection_enabled), and
    /// `update_placement_preview` describe.
    ///
    /// The lock mode is taken only when no lock command is pending, or when
    /// the snapshot names the pending one as the last applied lock command;
    /// the pending command is then cleared. Example: after `<C-l>` sends lock
    /// command `A` and shows `Locked`, a snapshot with `Normal` that names no
    /// lock command leaves `Locked`, and the next snapshot that names `A` sets
    /// its own lock mode.
    pub fn apply_render_snapshot(&mut self, render_snapshot: &RenderSnapshot) {
        self.settle_placement_command(
            render_snapshot
                .client_snapshot
                .last_applied_placement_command_id,
        );
        let is_placement_reconciled = self.placement_state.needs_placement_reconciliation;
        self.settle_floating_pane_move(
            render_snapshot
                .client_snapshot
                .last_applied_floating_pane_move_command_id,
        );
        self.set_frame_view(
            render_snapshot.client_snapshot.active_tab_id,
            render_snapshot.client_snapshot.focused_pane_id,
            render_snapshot.client_snapshot.active_tab_focused_pane_id,
            render_snapshot
                .session_snapshot
                .tabs_metadata
                .iter()
                .map(|tab_metadata| tab_metadata.tab_id)
                .collect(),
        );
        let is_lock_command_pending = self.pending_lock_command_id.is_some()
            && self.pending_lock_command_id
                != render_snapshot.client_snapshot.last_applied_lock_command_id;
        if !is_lock_command_pending {
            self.pending_lock_command_id = None;
            self.set_lock_mode(render_snapshot.client_snapshot.lock_mode);
        }
        self.note_active_tab(render_snapshot.client_snapshot.active_tab_id);
        self.set_mouse_selection_enabled(
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
        );
        self.update_dock_undock_selection(&render_snapshot.session_snapshot);
        self.update_placement_preview(
            render_snapshot.placement_preview.as_ref(),
            is_placement_reconciled,
        );
    }

    /// Follow the tiled pane that dock/undock mode selected into
    /// `session_snapshot`, the frame being applied:
    ///
    /// - A frame that draws the pane floating keeps it selected as the source
    ///   pane of pane placement mode. That mode previews the frame's active
    ///   tab with no target, until Esc, and the end of this attachment loop
    ///   pass names its preview to the session. A drag of the pane under way becomes that
    ///   mode's drag, and the pane is drawn where the frame floats it, moved
    ///   by the pointer's travel since the press
    ///   ([`rebase_floating_pane_handle_drag`](Self::rebase_floating_pane_handle_drag)).
    /// - A frame whose active tab holds no slot of the pane leaves nothing
    ///   selected.
    ///
    /// Does nothing when the mode selected no tiled pane. Example: after Enter
    /// on tiled pane `pane-7`, the frame that floats `pane-7` makes it the
    /// source pane of pane placement mode.
    fn update_dock_undock_selection(&mut self, session_snapshot: &SessionSnapshot) {
        let Some(tiled_pane_selection) = self
            .dock_undock_mode
            .and_then(|dock_undock_mode| dock_undock_mode.tiled_pane_selection)
        else {
            return;
        };
        let pane_id = tiled_pane_selection.pane_id;
        let floating_pane_slot = session_snapshot
            .floating_pane_slots
            .iter()
            .find(|floating_pane_slot| floating_pane_slot.pane_id == pane_id);
        if let Some(floating_pane_slot) = floating_pane_slot {
            self.dock_undock_mode = Some(DockUndockMode::default());
            self.placement_state.placement_mode = Some(PlacementMode {
                source_pane_id: pane_id,
                source_tab_id: None,
                destination_tab_id: session_snapshot.active_tab_snapshot.tab_id,
                placement_direction: Direction::Right,
                placement_target: None,
                pending_placement_command_id: None,
            });
            self.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
            self.placement_state.placement_drag = self
                .rebase_floating_pane_handle_drag(pane_id, floating_pane_slot.outer_rect.origin)
                .map(|press_position| PlacementDrag {
                    start_position: press_position,
                    has_pointer_moved: true,
                    is_insertion_drag: true,
                });
            return;
        }
        let is_pane_in_active_tab = session_snapshot
            .active_tab_snapshot
            .pane_slots
            .iter()
            .any(|pane_slot| pane_slot.pane_id == pane_id);
        if !is_pane_in_active_tab {
            self.dock_undock_mode = Some(DockUndockMode::default());
        }
    }

    /// Copy every field [`apply_render_snapshot`](Self::apply_render_snapshot)
    /// changes. The accepted placement preview is shared, not copied.
    pub(crate) fn build_viewer_restore_point(&self) -> ViewerRestorePoint {
        ViewerRestorePoint {
            lock_mode: self.lock_mode,
            pending_lock_command_id: self.pending_lock_command_id,
            is_mouse_selection_enabled: self.is_mouse_selection_enabled,
            pending_key_sequence: self.pending_key_sequence.clone(),
            tabline_peek: self.tabline_peek,
            active_tab_id: self.active_tab_id,
            focused_pane_id: self.focused_pane_id,
            active_tab_focused_pane_id: self.active_tab_focused_pane_id,
            visible_tab_ids: self.visible_tab_ids.clone(),
            placement_state: self.placement_state.clone(),
            pane_move_mode: self.pane_move_mode,
            sent_floating_pane_move: self.sent_floating_pane_move,
            dock_undock_mode: self.dock_undock_mode,
            floating_pane_tray: self.floating_pane_tray,
        }
    }

    /// Put back every field `viewer_restore_point` copied.
    pub(crate) fn restore_viewer(&mut self, viewer_restore_point: ViewerRestorePoint) {
        let ViewerRestorePoint {
            lock_mode,
            pending_lock_command_id,
            is_mouse_selection_enabled,
            pending_key_sequence,
            tabline_peek,
            active_tab_id,
            focused_pane_id,
            active_tab_focused_pane_id,
            visible_tab_ids,
            placement_state,
            pane_move_mode,
            sent_floating_pane_move,
            dock_undock_mode,
            floating_pane_tray,
        } = viewer_restore_point;
        self.lock_mode = lock_mode;
        self.pending_lock_command_id = pending_lock_command_id;
        self.is_mouse_selection_enabled = is_mouse_selection_enabled;
        self.pending_key_sequence = pending_key_sequence;
        self.tabline_peek = tabline_peek;
        self.active_tab_id = active_tab_id;
        self.focused_pane_id = focused_pane_id;
        self.active_tab_focused_pane_id = active_tab_focused_pane_id;
        self.visible_tab_ids = visible_tab_ids;
        self.placement_state = placement_state;
        self.pane_move_mode = pane_move_mode;
        self.sent_floating_pane_move = sent_floating_pane_move;
        self.dock_undock_mode = dock_undock_mode;
        self.floating_pane_tray = floating_pane_tray;
    }

    /// Record the view carried by the newest painted frame: its active tab,
    /// its focused pane, the focused pane in its active tab, and its tab ids.
    ///
    /// A frame that focuses the placement source pane sets the source tab to
    /// the frame's active tab: after `pane-123` moves from tab `#1` to tab
    /// `#2`, its accepted frame focuses it in `#2`, so closing `#1` leaves pane
    /// placement mode on.
    pub(crate) fn set_frame_view(
        &mut self,
        active_tab_id: TabId,
        focused_pane_id: Option<PaneId>,
        active_tab_focused_pane_id: Option<PaneId>,
        visible_tab_ids: Vec<TabId>,
    ) {
        let has_active_tab_changed = self.active_tab_id != Some(active_tab_id);
        let has_focused_pane_changed = self.focused_pane_id != focused_pane_id;
        let has_visible_tab_list_changed = self.visible_tab_ids != visible_tab_ids;
        let is_reconciling_submitted_placement =
            self.placement_state.needs_placement_reconciliation;
        if is_reconciling_submitted_placement {
            self.clear_placement_draft();
        }
        if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            if let Some(source_tab_id) = placement_mode.source_tab_id.as_mut() {
                if focused_pane_id == Some(placement_mode.source_pane_id) {
                    *source_tab_id = active_tab_id;
                }
            }
        }
        let should_cancel_placement = self.is_placement_mode_active()
            && !is_reconciling_submitted_placement
            && (has_active_tab_changed
                || (has_focused_pane_changed && !self.is_placement_source_pane(focused_pane_id))
                || (has_visible_tab_list_changed
                    && !visible_tab_ids.is_empty()
                    && self
                        .placement_state
                        .placement_mode
                        .as_ref()
                        .and_then(|placement_mode| placement_mode.source_tab_id)
                        .is_some_and(|source_tab_id| !visible_tab_ids.contains(&source_tab_id))));
        if should_cancel_placement {
            self.cancel_placement_mode();
        }
        self.active_tab_id = Some(active_tab_id);
        self.focused_pane_id = focused_pane_id;
        self.active_tab_focused_pane_id = active_tab_focused_pane_id;
        self.visible_tab_ids = visible_tab_ids;
    }

    /// Settle the placement confirmation that waits for the session when the
    /// newest frame names it as `last_applied_placement_command_id`: that frame
    /// shows the placement, and [`set_frame_view`](Self::set_frame_view)
    /// reconciles it.
    ///
    /// - Pane placement mode that lasts until Esc stays on with no target while
    ///   `stay-in-pane-placement-mode-after-placement` is `#true`, and ends
    ///   while it is `#false`.
    /// - Pane placement mode opened by a pane-handle drag
    ///   ([`PlacementModeLifetime::UntilDragEnds`]) ends, whatever the setting
    ///   says.
    /// - In dock/undock mode, the mode ends whatever the setting says.
    ///
    /// A frame that names another placement, or none, changes nothing: the
    /// confirmation stays pending until the session rejects it or a frame
    /// names it.
    fn settle_placement_command(&mut self, last_applied_placement_command_id: Option<CommandId>) {
        let Some(pending_placement_command_id) = self.get_pending_placement_command_id() else {
            return;
        };
        if last_applied_placement_command_id != Some(pending_placement_command_id) {
            return;
        }
        let is_placement_mode_active = self.is_placement_mode_active();
        let should_stay_in_pane_placement_mode = self.dock_undock_mode.is_none()
            && self
                .client_config
                .should_stay_in_pane_placement_mode_after_placement;
        self.clear_placement_draft();
        if is_placement_mode_active && !should_stay_in_pane_placement_mode {
            self.cancel_placement_mode();
        }
        self.placement_state.needs_placement_reconciliation = true;
    }

    /// Hold `placement_preview`, the preview the newest frame carries, when it
    /// names the source pane and the destination tab of pane placement mode.
    /// Any other preview, or none, leaves this viewer holding no preview.
    ///
    /// - An unavailable preview ends pane placement mode
    ///   ([`cancel_placement_mode`](Self::cancel_placement_mode)).
    /// - An available preview sets the mode's source tab to the tab that
    ///   holds the source pane, or to `None` for a floating source pane.
    /// - A preview that differs from the one held clears a target that the
    ///   session refuses in it ([`PlacementSnapshot::build_placed_preview`]).
    ///   A target the session accepts in it stays selected.
    /// - A preview that differs from the one held, or a frame that reconciles
    ///   this viewer's placement confirmation (`is_placement_reconciled`),
    ///   selects the whole destination tab
    ///   ([`select_whole_tab_insertion_target`]).
    ///
    /// Two previews with equal values count as the same preview.
    ///
    /// [`select_whole_tab_insertion_target`]: Self::select_whole_tab_insertion_target
    fn update_placement_preview(
        &mut self,
        placement_preview: Option<&Arc<PlacementPreview>>,
        is_placement_reconciled: bool,
    ) {
        let placement_preview_target = self.find_placement_preview_target();
        let placement_preview = placement_preview
            .filter(|placement_preview| {
                let (source_pane_id, destination_tab_id) = placement_preview.get_placement_target();
                placement_preview_target
                    == Some(PanePlacementPreviewTarget {
                        source_pane_id,
                        destination_tab_id,
                    })
            })
            .cloned();
        let is_preview_changed = placement_preview.as_ref() != self.find_held_placement_preview();
        if is_preview_changed || placement_preview.is_none() {
            self.placement_state.placement_preview = placement_preview;
        }
        if self.cancel_placement_mode_if_preview_unavailable() {
            return;
        }
        if is_preview_changed {
            self.apply_changed_placement_preview();
        }
        if is_preview_changed || is_placement_reconciled {
            self.select_whole_tab_insertion_target();
        }
    }

    /// End pane placement mode when the held preview is unavailable
    /// ([`PlacementPreview::Unavailable`]), and return whether it did. A
    /// placement confirmation that waits for the session keeps the mode on
    /// ([`cancel_placement_mode`](Self::cancel_placement_mode)).
    fn cancel_placement_mode_if_preview_unavailable(&mut self) -> bool {
        let is_preview_unavailable = self
            .find_held_placement_preview()
            .is_some_and(|placement_preview| placement_preview.find_placement_snapshot().is_none());
        if !is_preview_unavailable || self.is_placement_confirmation_pending() {
            return false;
        }
        self.cancel_placement_mode();
        true
    }

    /// Apply the held preview, new to this viewer, to pane placement mode: the
    /// mode's source tab becomes the tab that holds the source pane, or `None`
    /// for a floating source pane, and a target that the session refuses in
    /// the preview ([`Self::update_placement_target_outcome`]) clears. A
    /// target that waits for the session stays. Does nothing with no
    /// available preview.
    fn apply_changed_placement_preview(&mut self) {
        let Some(placement_snapshot) = self.get_placement_snapshot() else {
            return;
        };
        let source_tab_id = placement_snapshot
            .find_source_tab_snapshot()
            .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.tab_id);
        self.update_placement_target_outcome();
        let is_target_refused = self.find_placement_refusal().is_some();
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        placement_mode.source_tab_id = source_tab_id;
        if is_target_refused && placement_mode.pending_placement_command_id.is_none() {
            placement_mode.placement_target = None;
        }
    }

    /// Enter local placement mode for the focused pane in the active tab,
    /// until Esc, also while a floating pane takes this viewer's input. The
    /// preview starts on the active tab: `<C-p> m` on tab `#1` of `[#1, #2]`
    /// previews `#1`. Returns `false`, changing nothing, when dock/undock mode
    /// is on, placement mode is already on, a placement waits for the session,
    /// floating pane move mode waits for the move it sent, or the active tab
    /// focuses no pane.
    pub(crate) fn begin_placement_mode(&mut self) -> bool {
        if self.dock_undock_mode.is_some() {
            return false;
        }
        let (Some(source_pane_id), Some(source_tab_id)) =
            (self.active_tab_focused_pane_id, self.active_tab_id)
        else {
            return false;
        };
        self.open_placement_mode(
            source_pane_id,
            Some(source_tab_id),
            source_tab_id,
            PlacementModeLifetime::UntilCancelled,
        )
    }

    /// Enter local placement mode for `source_pane_id` from a press on its
    /// painted placement handle, until the drag ends. Returns `false`,
    /// changing nothing, when placement mode is already on, a placement waits
    /// for the session, or floating pane move mode waits for the move it sent.
    pub(crate) fn begin_mouse_placement_mode(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: TabId,
    ) -> bool {
        self.open_placement_mode(
            source_pane_id,
            Some(source_tab_id),
            source_tab_id,
            PlacementModeLifetime::UntilDragEnds,
        )
    }

    /// Start a placement of `source_pane_id`, held in the layout of
    /// `source_tab_id` or floating with `None`, that previews
    /// `destination_tab_id` with no target, ends the move chooser or floating
    /// pane move mode, drops any open key sequence, tab hover, placement
    /// draft, and every gesture a held button drives
    /// ([`end_held_mouse_gestures`](Self::end_held_mouse_gestures)), and
    /// returns `true`. Returns `false`, changing nothing, when placement mode
    /// is already on, a placement waits for the session, or floating pane move
    /// mode waits for the move it sent.
    fn open_placement_mode(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: Option<TabId>,
        destination_tab_id: TabId,
        placement_mode_lifetime: PlacementModeLifetime,
    ) -> bool {
        if self.is_placement_mode_active()
            || self.is_placement_confirmation_pending()
            || self.is_floating_pane_move_pending()
        {
            return false;
        }
        self.placement_state.placement_mode = Some(PlacementMode {
            source_pane_id,
            source_tab_id,
            destination_tab_id,
            placement_direction: Direction::Right,
            placement_target: None,
            pending_placement_command_id: None,
        });
        self.placement_state.placement_mode_lifetime = placement_mode_lifetime;
        self.pane_move_mode = None;
        self.end_held_mouse_gestures();
        self.pending_key_sequence = None;
        self.clear_placement_draft();
        true
    }

    /// Whether this viewer's keyboard or active mouse drag belongs to placement.
    #[must_use]
    pub(crate) fn is_placement_mode_active(&self) -> bool {
        self.placement_state
            .placement_mode
            .as_ref()
            .is_some_and(|placement_mode| {
                placement_mode.pending_placement_command_id.is_none()
                    || self.placement_state.placement_mode_lifetime
                        == PlacementModeLifetime::UntilCancelled
            })
    }

    /// Return whether pane placement draws its subdued panes and in-pane messages,
    /// with no hover tint and no placement handle.
    /// It stays `true` while a submitted placement command waits for the
    /// session's answer. It is `false` while dock/undock mode selected a
    /// floating pane that has no dock target and no drag moving it.
    #[must_use]
    pub(crate) fn is_pane_placement_visible(&self) -> bool {
        (self.is_placement_mode_active() || self.is_placement_confirmation_pending())
            && !self.is_floating_dock_selection_idle()
    }

    /// Whether dock/undock mode selected a floating pane that has no dock
    /// target, no drag moving it, and no dock waiting for the session. The
    /// frame then shows no placement preview.
    #[must_use]
    pub(crate) fn is_floating_dock_selection_idle(&self) -> bool {
        self.dock_undock_mode.is_some()
            && self.placement_state.placement_drag.is_none()
            && self
                .placement_state
                .placement_mode
                .as_ref()
                .is_some_and(|placement_mode| {
                    placement_mode.placement_target.is_none()
                        && placement_mode.pending_placement_command_id.is_none()
                })
    }

    /// Return whether `focused_pane_id` names the source pane of the active
    /// placement. A mouse pickup of `pane-123` focuses `pane-123`, so
    /// `Some(pane-123)` returns `true`. `None` returns `false`.
    #[must_use]
    fn is_placement_source_pane(&self, focused_pane_id: Option<PaneId>) -> bool {
        self.placement_state
            .placement_mode
            .as_ref()
            .is_some_and(|placement_mode| focused_pane_id == Some(placement_mode.source_pane_id))
    }

    /// The checked target currently selected by placement mode.
    #[must_use]
    pub(crate) fn get_placement_target(&self) -> Option<&TiledPlacementTarget> {
        self.placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.placement_target.as_ref())
    }

    /// Return the pane selected as the source of the active placement.
    #[must_use]
    pub(crate) fn get_placement_source_pane_id(&self) -> Option<PaneId> {
        self.placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| placement_mode.source_pane_id)
    }

    /// Return the tab selected as the active placement destination.
    #[must_use]
    pub(crate) fn get_placement_destination_tab_id(&self) -> Option<TabId> {
        self.placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| placement_mode.destination_tab_id)
    }

    /// Select a placement destination tab without changing session focus.
    /// Clear the prior target and the tab hover; the held preview names the
    /// prior tab and no longer counts. Keep an active drag and its insertion
    /// choice. Does nothing while a placement waits for the session, with no
    /// placement mode, or when `destination_tab_id` is already the
    /// destination.
    pub(crate) fn select_placement_destination_tab(&mut self, destination_tab_id: TabId) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return;
        };
        if placement_mode.destination_tab_id == destination_tab_id {
            return;
        }
        placement_mode.destination_tab_id = destination_tab_id;
        placement_mode.placement_target = None;
        self.placement_state.placement_tab_hover = None;
    }

    /// Start or clear the placement tab hover under the pointer.
    pub(crate) fn update_placement_tab_hover(
        &mut self,
        hovered_tab_id: Option<TabId>,
        current_time: Instant,
    ) {
        if !self.is_placement_mode_active() {
            self.placement_state.placement_tab_hover = None;
            return;
        }
        match hovered_tab_id {
            Some(tab_id)
                if self
                    .placement_state
                    .placement_tab_hover
                    .is_some_and(|placement_tab_hover| placement_tab_hover.tab_id == tab_id) => {}
            Some(tab_id) => {
                self.placement_state.placement_tab_hover = Some(PlacementTabHover {
                    tab_id,
                    entered_at: current_time,
                    is_destination_tab_selected: false,
                });
            }
            None => self.placement_state.placement_tab_hover = None,
        }
    }

    /// Return the time until the current placement tab hover selects its tab
    /// as the destination. While an input event this terminal's input thread
    /// read has not reached the loop ([`InputReadClock::has_untaken_input`]),
    /// the answer is `None`: that event's arrival wakes the loop.
    #[must_use]
    pub(crate) fn compute_next_placement_tab_hover_wakeup(
        &self,
        current_time: Instant,
    ) -> Option<Duration> {
        if self.input_read_clock.has_untaken_input() {
            return None;
        }
        let placement_tab_hover = self.placement_state.placement_tab_hover?;
        (!placement_tab_hover.is_destination_tab_selected).then(|| {
            placement_tab_hover
                .entered_at
                .checked_add(PLACEMENT_TAB_HOVER_DELAY_DURATION)
                .map_or(Duration::ZERO, |preview_time| {
                    preview_time.saturating_duration_since(current_time)
                })
        })
    }

    /// Select the tab under a placement tab hover as the destination once the
    /// hover has lasted
    /// [`PLACEMENT_TAB_HOVER_DELAY_DURATION`] at `current_time`
    /// ([`select_placement_destination_tab`](Self::select_placement_destination_tab)).
    /// A hover selects its tab once.
    pub(crate) fn expire_placement_tab_hover(&mut self, current_time: Instant) {
        let Some(placement_tab_hover) = self.placement_state.placement_tab_hover.as_mut() else {
            return;
        };
        let Some(selection_time) = placement_tab_hover
            .entered_at
            .checked_add(PLACEMENT_TAB_HOVER_DELAY_DURATION)
        else {
            return;
        };
        if placement_tab_hover.is_destination_tab_selected || current_time < selection_time {
            return;
        }
        placement_tab_hover.is_destination_tab_selected = true;
        let destination_tab_id = placement_tab_hover.tab_id;
        self.select_placement_destination_tab(destination_tab_id);
    }

    /// Select `source_pane_id` in `source_tab_id` as the pane to place, without
    /// changing session focus, and return `true`. The held preview names the
    /// prior pane and no longer counts. Returns `false`, changing nothing,
    /// while a placement waits for the session, with no placement mode, or
    /// when `source_pane_id` is already the source pane, in any tab.
    pub(crate) fn select_placement_source_pane(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: TabId,
    ) -> bool {
        if self.is_placement_confirmation_pending() {
            return false;
        }
        let Some(placement_mode) = self.placement_state.placement_mode.as_mut() else {
            return false;
        };
        if placement_mode.source_pane_id == source_pane_id {
            return false;
        }
        placement_mode.source_pane_id = source_pane_id;
        placement_mode.source_tab_id = Some(source_tab_id);
        if !self
            .visible_tab_ids
            .contains(&placement_mode.destination_tab_id)
        {
            placement_mode.destination_tab_id = source_tab_id;
        }
        placement_mode.placement_target = None;
        self.placement_state.placement_drag = None;
        true
    }

    /// Record the start of a viewer-owned placement drag.
    pub(crate) fn begin_placement_drag(&mut self, start_position: Point, is_insertion_drag: bool) {
        if self.is_placement_mode_active() && !self.is_placement_confirmation_pending() {
            self.placement_state.placement_drag = Some(PlacementDrag {
                start_position,
                has_pointer_moved: false,
                is_insertion_drag,
            });
        }
    }

    /// Return whether this viewer is dragging a pane for placement.
    pub(crate) fn is_placement_drag_active(&self) -> bool {
        self.placement_state.placement_drag.is_some()
    }

    /// Record movement away from the drag origin and return whether it has moved
    /// at least once during this drag.
    pub(crate) fn update_placement_drag_movement(&mut self, position: Point) -> bool {
        let Some(placement_drag) = self.placement_state.placement_drag.as_mut() else {
            return false;
        };
        placement_drag.has_pointer_moved |= placement_drag.start_position != position;
        placement_drag.has_pointer_moved
    }

    /// Return whether the active placement mode ends when its drag ends.
    #[must_use]
    pub(crate) fn should_placement_mode_end_with_drag(&self) -> bool {
        self.is_placement_mode_active()
            && self.placement_state.placement_mode_lifetime == PlacementModeLifetime::UntilDragEnds
    }

    /// End the viewer-owned placement drag.
    pub(crate) fn end_placement_drag(&mut self) {
        self.placement_state.placement_drag = None;
    }

    /// Whether the placement preview is loading: pane placement mode is on,
    /// and this viewer holds no preview of its source pane and destination tab
    /// yet.
    #[must_use]
    pub(crate) fn is_placement_preview_loading(&self) -> bool {
        self.placement_state.placement_mode.is_some()
            && self.find_held_placement_preview().is_none()
    }

    /// End pane placement mode and drop its preview, tab hover, open key
    /// sequence, and placement draft. Does nothing while a placement command
    /// waits for the session.
    pub(crate) fn cancel_placement_mode(&mut self) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        self.placement_state.placement_mode = None;
        self.placement_state.placement_preview = None;
        self.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
        self.pending_key_sequence = None;
        self.clear_placement_draft();
    }

    /// Drop the tab hover, the drag, and any due reconciliation. A mode that
    /// lasts until its drag ends closes when a placement command is pending.
    /// Any other mode stays on with no target and no pending placement
    /// command.
    pub(crate) fn clear_placement_draft(&mut self) {
        self.placement_state.needs_placement_reconciliation = false;
        self.placement_state.placement_drag = None;
        self.placement_state.placement_tab_hover = None;
        let is_drag_confirmation_pending = self.is_placement_confirmation_pending()
            && self.placement_state.placement_mode_lifetime == PlacementModeLifetime::UntilDragEnds;
        if is_drag_confirmation_pending {
            self.placement_state.placement_mode = None;
            self.placement_state.placement_preview = None;
            self.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
        } else if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            placement_mode.placement_target = None;
            placement_mode.pending_placement_command_id = None;
        }
    }

    /// Return the id of the placement this client confirmed and the session
    /// has not yet answered, or `None` when no confirmation waits.
    fn get_pending_placement_command_id(&self) -> Option<CommandId> {
        self.placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.pending_placement_command_id)
    }

    /// Return whether `command_id` is the placement this client confirmed and
    /// the session has not yet answered. Another viewer's placement returns
    /// `false`.
    pub(crate) fn is_pending_placement_command(&self, command_id: CommandId) -> bool {
        self.get_pending_placement_command_id() == Some(command_id)
    }

    /// Drop the command `command_id` names, which the session rejected, and
    /// return `true`: a sent floating pane move
    /// ([`reject_floating_pane_move`](Self::reject_floating_pane_move)), the
    /// undock that dock/undock mode sent
    /// ([`reject_undock_command`](Self::reject_undock_command)), or the
    /// pending placement confirmation. After a rejected confirmation, pane
    /// placement mode that lasts until Esc stays on with the preview it holds,
    /// and selects the whole destination tab
    /// ([`select_whole_tab_insertion_target`](Self::select_whole_tab_insertion_target));
    /// a mode that lasts until its drag ends closes. A held preview that is
    /// unavailable then ends the mode. Returns `false`, changing nothing, when
    /// `command_id` names none of them.
    pub(crate) fn reject_placement_command(&mut self, command_id: CommandId) -> bool {
        if self.reject_floating_pane_move(command_id) || self.reject_undock_command(command_id) {
            return true;
        }
        if !self.is_pending_placement_command(command_id) {
            return false;
        }
        self.clear_placement_draft();
        if !self.cancel_placement_mode_if_preview_unavailable() {
            self.select_whole_tab_insertion_target();
        }
        true
    }

    /// Whether a placement confirmation awaits the session's answer.
    /// Releasing over `pane-123` keeps this true until the session rejects the
    /// placement, or a frame names it as the client's last applied placement.
    #[must_use]
    pub(crate) fn is_placement_confirmation_pending(&self) -> bool {
        self.get_pending_placement_command_id().is_some()
    }

    /// Mark a pending placement confirmation for reconciliation with the next
    /// frame, and drop the undock that dock/undock mode waits for: the next
    /// frame shows whether it ran ([`update_dock_undock_selection`]). A resync,
    /// a restart, or a redial calls it: the session's answer to either may be
    /// lost. The next frame settles the confirmation when it names it
    /// (`settle_placement_command`), and else clears the placement draft
    /// ([`set_frame_view`](Self::set_frame_view)).
    ///
    /// [`update_dock_undock_selection`]: Self::update_dock_undock_selection
    pub(crate) fn prepare_placement_reconciliation(&mut self) {
        if let Some(tiled_pane_selection) = self
            .dock_undock_mode
            .as_mut()
            .and_then(|dock_undock_mode| dock_undock_mode.tiled_pane_selection.as_mut())
        {
            tiled_pane_selection.pending_undock_command_id = None;
        }
        self.placement_state.needs_placement_reconciliation =
            self.is_placement_confirmation_pending();
    }

    /// The placement target this viewer previews: the source pane and the
    /// destination tab of pane placement mode, or `None` with no placement
    /// mode. The end of each attachment loop pass names it to the session
    /// (`SetPanePlacementPreview`) when it differs from the target named last
    /// on the link.
    #[must_use]
    pub(crate) fn find_placement_preview_target(&self) -> Option<PanePlacementPreviewTarget> {
        self.placement_state
            .placement_mode
            .as_ref()
            .map(|placement_mode| PanePlacementPreviewTarget {
                source_pane_id: placement_mode.source_pane_id,
                destination_tab_id: placement_mode.destination_tab_id,
            })
    }

    /// The held placement preview when it names the source pane and the
    /// destination tab of pane placement mode, or `None`.
    pub(crate) fn find_held_placement_preview(&self) -> Option<&Arc<PlacementPreview>> {
        let placement_preview_target = self.find_placement_preview_target()?;
        self.placement_state
            .placement_preview
            .as_ref()
            .filter(|placement_preview| {
                let (source_pane_id, destination_tab_id) = placement_preview.get_placement_target();
                placement_preview_target
                    == PanePlacementPreviewTarget {
                        source_pane_id,
                        destination_tab_id,
                    }
            })
    }

    /// The layouts of the held placement preview of pane placement mode's
    /// source pane and destination tab, or `None` while this viewer holds no
    /// available preview of them.
    #[must_use]
    pub fn get_placement_snapshot(&self) -> Option<&PlacementSnapshot> {
        self.find_held_placement_preview()?
            .find_placement_snapshot()
    }

    /// Hold the session's answer to placement mode's selected target in the
    /// held available preview ([`Self::find_held_placement_preview`]), built
    /// with [`PlacementSnapshot::build_placed_preview`] only when the preview
    /// or the target differs from the answer held. With no available held
    /// preview or no target, no answer is held.
    pub(crate) fn update_placement_target_outcome(&mut self) {
        let held_placement_snapshot =
            self.find_held_placement_preview()
                .and_then(|held_placement_preview| {
                    held_placement_preview
                        .find_placement_snapshot()
                        .map(|placement_snapshot| (held_placement_preview, placement_snapshot))
                });
        let (Some((held_placement_preview, placement_snapshot)), Some(placement_target)) =
            (held_placement_snapshot, self.get_placement_target())
        else {
            self.placement_state.placement_target_outcome = None;
            return;
        };
        let is_outcome_current = self
            .placement_state
            .placement_target_outcome
            .as_ref()
            .is_some_and(|placement_target_outcome| {
                placement_target_outcome.placement_preview == *held_placement_preview
                    && placement_target_outcome.placement_target == *placement_target
            });
        if is_outcome_current {
            return;
        }
        let placement_target_outcome = PlacementTargetOutcome {
            placement_preview: Arc::clone(held_placement_preview),
            placement_target: placement_target.clone(),
            placed_preview: placement_snapshot.build_placed_preview(placement_target),
        };
        self.placement_state.placement_target_outcome = Some(placement_target_outcome);
    }

    /// The answer [`Self::update_placement_target_outcome`] last built.
    #[must_use]
    pub(crate) fn get_placement_target_outcome(&self) -> Option<&PlacementTargetOutcome> {
        self.placement_state.placement_target_outcome.as_ref()
    }

    /// The session's refusal of the selected target in the answer
    /// [`Self::update_placement_target_outcome`] last built. `None` with no
    /// answer, and for a target the session accepts.
    #[must_use]
    pub(crate) fn find_placement_refusal(&self) -> Option<&PlacementPreviewError> {
        self.get_placement_target_outcome()
            .and_then(|placement_target_outcome| {
                placement_target_outcome.placed_preview.as_ref().err()
            })
    }

    /// The settings this viewer owns.
    #[must_use]
    pub fn get_client_config(&self) -> &ClientConfig {
        &self.client_config
    }

    /// The chrome colors every koshi-owned surface in this client's frames is
    /// painted with.
    #[must_use]
    pub fn get_theme(&self) -> &Theme {
        &self.theme
    }

    /// Whether this viewer grabs the mouse for text selection, as the session
    /// last reported it.
    #[must_use]
    pub fn is_mouse_selection_enabled(&self) -> bool {
        self.is_mouse_selection_enabled
    }

    /// Set whether this viewer grabs the mouse for text selection.
    ///
    /// Called when the session reports the mode for this viewer, either as an
    /// event or in the frame an attached viewer reads. The session owns the
    /// mode; this only moves the viewer's copy of it, which mouse routing
    /// reads. A change ends pane placement mode, the move chooser, floating
    /// pane move mode that waits for no move, dock/undock mode that waits for
    /// no undock or dock, and the floating pane tray.
    pub fn set_mouse_selection_enabled(&mut self, is_mouse_selection_enabled: bool) {
        if self.is_mouse_selection_enabled != is_mouse_selection_enabled {
            if self.is_placement_mode_active() {
                self.cancel_placement_mode();
            }
            self.close_pane_move_mode();
            self.close_dock_undock_mode();
            self.close_floating_pane_tray();
        }
        self.is_mouse_selection_enabled = is_mouse_selection_enabled;
    }
}

/// Whether the pane `client_snapshot` focuses is floating: one of
/// `session_snapshot`'s
/// [`floating_panes`](SessionSnapshot::floating_panes). `false` when the
/// client focuses no pane.
pub(crate) fn is_focused_pane_floating(
    session_snapshot: &SessionSnapshot,
    client_snapshot: &ClientSnapshot,
) -> bool {
    client_snapshot
        .focused_pane_id
        .is_some_and(|focused_pane_id| session_snapshot.is_floating_pane(focused_pane_id))
}
