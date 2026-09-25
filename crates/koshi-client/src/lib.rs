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

pub mod input;

pub mod mouse;

/// The outer terminal an attached client owns: its viewer, its input thread,
/// and painting frames into it.
pub(crate) mod terminal;

pub mod theme;

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_config::conflict::{
    build_keymap_layers, detect_conflicts, ConflictReport, KeymapVerdict,
};
use koshi_config::hints::{HintBinding, KeymapHintCatalog, KeymapHints};
use koshi_config::layer::{
    ConfigLayers, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::types::ClientConfig;
use koshi_core::action::{MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT};
use koshi_core::command::{Command, PanePlacementTarget};
use koshi_core::geometry::Direction;
use koshi_core::key::PendingKeySequence;
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_core::{
    event::Event,
    geometry::{PaneArea, Point, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
};
use koshi_ipc::placement::PanePlacementSnapshot;
use koshi_ipc::protocol::IpcErrorCode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::region::solve_core_regions;
use koshi_renderer::snapshot::{Delivery, Reconnecting, RenderSnapshot};
use koshi_renderer::theme::Theme;

use crate::mouse::{LastPress, MouseCapture, ResizeDrag, SelectionDrag, TablineDrag, TablinePeek};

#[cfg(test)]
mod tests;

/// Compute the pane area left by the compiled-in navigator and hint regions.
///
/// An `80x24` viewport reports `Reported(80x22)`. A viewport shorter than the
/// two rows reports zero rows instead of an invalid negative size.
#[must_use]
pub(crate) fn compute_core_pane_area(viewport: Size) -> PaneArea {
    PaneArea::Reported(solve_core_regions(viewport).pane_rect.cell_size)
}

/// One pending read-only placement preview request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementReadRequest {
    /// The request id sent on the attached event stream.
    pub(crate) request_id: u64,
    /// The source pane named by the request.
    pub(crate) source_pane_id: PaneId,
    /// The destination tab named by the request.
    pub(crate) destination_tab_id: TabId,
    /// The session placement revision seen before the request was sent.
    pub(crate) session_placement_revision: u64,
    /// The client placement revision seen before the request was sent.
    pub(crate) client_placement_revision: u64,
    /// The monotonic time after which the request no longer accepts a reply.
    pub(crate) expires_at: Instant,
}

/// How long a placement preview read may wait for a reply.
pub(crate) const PLACEMENT_READ_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the pointer must remain over a placement tab before it is previewed.
pub(crate) const PLACEMENT_TAB_HOVER_DELAY_DURATION: Duration = Duration::from_millis(200);

/// The viewer-owned state for one placement interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementMode {
    /// The pane being placed. A left press on another pane in placement mode
    /// selects that pane instead.
    pub(crate) source_pane_id: PaneId,
    /// The tab that owns the source pane. A painted frame that focuses the
    /// source pane sets it to that frame's active tab, and each accepted
    /// preview sets it to the tab that preview names: after `pane-123` moves
    /// from tab `#1` to tab `#2`, both set `#2`.
    pub(crate) source_tab_id: TabId,
    /// The tab currently being previewed.
    pub(crate) destination_tab_id: TabId,
    /// The edge used by the last insertion selection.
    pub(crate) placement_direction: Direction,
    /// The checked target selected from the retained destination snapshot.
    pub(crate) placement_target: Option<PanePlacementTarget>,
    /// The command Enter or a mouse drop sent for `placement_target`, until the
    /// session answers it. `None` before a confirmation.
    pub(crate) pending_placement_command: Option<PendingPlacementCommand>,
}

/// A placement command this client sent and the session has not yet answered
/// with a rejection or an accepted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingPlacementCommand {
    /// The id the command carried.
    pub(crate) command_id: CommandId,
    /// Whether `PanePlacementCommitted` named `command_id`. The next frame with
    /// new placement revisions then shows this placement.
    pub(crate) is_committed: bool,
}

/// One viewer-owned mouse drag that may confirm a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementDrag {
    /// The screen position where the drag began.
    pub(crate) start_position: Point,
    /// Shift+drag selects insertion targets; a plain drag selects swaps.
    pub(crate) is_insertion_drag: bool,
}

/// A placement tab hover waiting for its one preview read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementTabHover {
    /// The tab under the pointer.
    pub(crate) tab_id: TabId,
    /// When the pointer entered the tab.
    pub(crate) entered_at: Instant,
    /// Whether the hover already requested its preview.
    pub(crate) is_preview_requested: bool,
}

/// How long the current placement mode lasts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PlacementModeLifetime {
    /// Until Esc. `core:begin-pane-placement` (`<C-p> m` by default) opens
    /// this lifetime. A mouse drop the session accepts switches to it while
    /// `stay-in-pane-placement-mode-after-placement` is `#true`. With that
    /// setting `#false`, an accepted placement ends it.
    #[default]
    UntilCancelled,
    /// Until the pane-handle drag that opened it ends.
    UntilDragEnds,
}

/// One viewer's pane placement mode, its preview reads, and the placement
/// revisions of the newest painted frame. A clone shares the accepted preview.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlacementState {
    /// The latest session placement revision seen in a painted frame.
    session_placement_revision: u64,
    /// The latest client placement revision seen in a painted frame.
    client_placement_revision: u64,
    /// The one placement preview request this client has sent, if any.
    placement_read_request: Option<PlacementReadRequest>,
    /// The newest destination requested while one read is in flight.
    queued_placement_read: Option<(PaneId, TabId)>,
    /// The newest accepted placement preview for this client.
    placement_snapshot: Option<Arc<PanePlacementSnapshot>>,
    /// The local placement interaction, if the viewer is choosing a destination.
    placement_mode: Option<PlacementMode>,
    /// How long the current placement interaction lasts.
    placement_mode_lifetime: PlacementModeLifetime,
    /// Whether [`Client::set_frame_view`] reconciles a submitted placement. It
    /// is set when a frame shows this viewer's committed placement, and when a
    /// resync, restart, or redial lost the session's answer. The reconciling
    /// view clears the placement draft, and its tab or focus change does not
    /// cancel pane placement mode.
    needs_placement_reconciliation: bool,
    /// Whether a fresh preview read of the source pane and destination tab is
    /// due. The end of the attachment loop pass sends it.
    needs_placement_preview_refresh: bool,
    /// The mouse drag that placement mode currently owns.
    placement_drag: Option<PlacementDrag>,
    /// The tab hover waiting to request a placement preview.
    placement_tab_hover: Option<PlacementTabHover>,
}

/// The viewer fields [`Client::apply_render_snapshot`] changes, copied before
/// one frame is applied. [`Client::restore_viewer`] puts them back.
pub(crate) struct ViewerRestorePoint {
    /// The base input mode.
    lock_mode: LockMode,
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
    /// The tab ids of the newest painted frame.
    visible_tab_ids: Vec<TabId>,
    /// Pane placement mode, its preview reads, and the placement revisions.
    placement_state: PlacementState,
}

/// What placement mode does with one owned key or mouse event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlacementInputAction {
    /// Read the selected destination tab and optionally focus its source pane.
    ReadPlacement {
        /// The pane a mouse pickup focuses, or `None` when focus stays where it is.
        pane_id_to_focus: Option<PaneId>,
        /// The pane the preview places.
        source_pane_id: PaneId,
        /// The tab the preview shows.
        destination_tab_id: TabId,
    },
    /// Send the checked placement command that placement mode recorded as
    /// pending under `command_id`.
    SubmitPlacement {
        /// The id the pending placement command carries.
        command_id: CommandId,
        /// The `PlacePane` command to send.
        command: Command,
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
    viewport: Size,
    /// Receiving end of this client's event subscription, read by
    /// [`apply_events`](Self::apply_events). A viewer subscribed to a session in
    /// this process is fed by that session's bounded fan-out: live events, and
    /// the fresh frame the session sends after this subscriber's queue
    /// overflowed. A viewer attached over a connection is handed a receiver with
    /// no sender — its frames arrive on the connection — so nothing is ever
    /// delivered here.
    delivery_receiver: Receiver<Delivery>,
    /// This viewer's stored config overrides, one layer per config file, as
    /// [`load_startup_config`](Self::load_startup_config) last left them. A
    /// refused `keybinding.kdl` leaves its layer empty.
    config_layers: ConfigLayers,
    /// The settings this viewer owns, folded from [`config_layers`](Self::config_layers).
    client_config: ClientConfig,
    /// The chrome colors [`client_config`](Self::client_config)'s theme resolves to. Held
    /// resolved, so a frame reads them by borrow.
    theme: Theme,
    /// The keymap this viewer resolves its own keys against, built from
    /// [`client_config`](Self::client_config)'s keybindings and the action table.
    keymap_catalog: KeymapHintCatalog,
    /// The action table a bound name is checked against — for the hint bar's
    /// labels and the `continuous` flag a repeat-capable binding re-arms on.
    /// Dispatch itself happens on the session, against its own table.
    registry: ActionRegistry,
    /// This viewer's base input mode. It decides what a key means when no local
    /// submode owns the keyboard. Placement mode overlays this value without
    /// changing it, so a placement interaction can return to Normal or Locked.
    /// The session keeps its own copy, which `koshi lock --client` reaches and
    /// `koshi list-clients` reports.
    lock_mode: LockMode,
    /// Whether this viewer grabs the mouse for text selection. It decides what
    /// a press means before anything is sent. The session keeps its own copy,
    /// which the frame carries for the mode indicator and the hint bar's label.
    is_mouse_selection_enabled: bool,
    /// The multi-chord binding being typed, if any. Held chords belong to
    /// koshi and never reach a pane.
    pending_key_sequence: Option<PendingKeySequence>,
    /// The most recent mouse press, which is what tells a double click from two
    /// separate clicks. `None` before this viewer has pressed anything.
    last_mouse_press: Option<LastPress>,
    /// The pane a forwarded press captured, and the button that pressed it.
    /// While a button is held, its drags and its release go to this pane even as
    /// the pointer leaves it, and a drag or release with no capture is not
    /// forwarded. Set when this viewer forwards the press; cleared on the next
    /// release.
    ///
    /// The stored button is the reliable one — a press always names its button,
    /// while some terminals report every drag and release as the left button.
    mouse_capture: Option<MouseCapture>,
    /// The pane-border drag under way, held only between the press on a border
    /// that begins it and the release that ends it.
    resize_drag: Option<ResizeDrag>,
    /// The tab-strip peek-drag under way, held only between the press on the
    /// bare strip that begins it and the release that ends it.
    tabline_drag: Option<TablineDrag>,
    /// Where this viewer's tab strip is scrolled, and the tab it was scrolled
    /// on. `None` follows the active tab. A [`TablinePeek`] records the active
    /// tab and the first visible tab index. The peek belongs to the tab it was
    /// made on, and [`Client::note_active_tab`] throws it away as soon as the
    /// viewer sees a frame on another tab.
    tabline_peek: Option<TablinePeek>,
    /// The text-selection drag under way, held only between the press on a
    /// pane's content that begins it and the release that ends it. The highlight
    /// it produces lives on the session and outlives it.
    selection_drag: Option<SelectionDrag>,
    /// The line the pane's view showed on its top row when the last edge-scroll
    /// step was asked for, awaiting the session's report of where the view
    /// landed. Set only for a scroll the selection drag's timer asked for.
    selection_scroll_origin_row_index: Option<u64>,
    /// The pane this viewer's pointer is over, or `None` when it is over chrome
    /// or off every pane. The renderer draws an unfocused hovered pane in the
    /// hover color, so the wheel's target is visible before the wheel turns.
    hovered_pane_id: Option<PaneId>,
    /// The pane whose top border currently exposes the placement handle.
    placement_handle_pane_id: Option<PaneId>,
    /// Where this viewer's dialing stands while it has no link to the session,
    /// and `None` while it has one. The tabline draws
    /// `RECONNECTING (attempt 4, retry in 8s)` while it holds a
    /// `Reconnecting { attempt: 4, retry_in_seconds: 8 }`.
    reconnecting: Option<Reconnecting>,
    /// This viewer's pane placement mode, its preview reads, and the placement
    /// revisions of the newest painted frame.
    placement_state: PlacementState,
    /// The active tab in the newest painted frame.
    active_tab_id: Option<TabId>,
    /// The focused pane in the newest painted frame.
    focused_pane_id: Option<PaneId>,
    /// The tab ids in display order in the newest painted frame.
    visible_tab_ids: Vec<TabId>,
    /// Restores the outer terminal when the client ends or the process
    /// panics. Held to be dropped with the client; nothing reads it.
    _terminal_cleanup_guard: TerminalCleanupGuard,
}

impl Client {
    /// Build a client from its id, its terminal's current size, the receiver
    /// the session handed out for it, and the outer-terminal cleanup guard.
    ///
    /// It starts on the built-in defaults: no stored config layers, the stock
    /// palette, and the shipped keymap. The files the user wrote arrive
    /// through [`load_startup_config`](Self::load_startup_config).
    #[must_use]
    pub fn from_client_id_and_viewport(
        client_id: ClientId,
        viewport: Size,
        delivery_receiver: Receiver<Delivery>,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        let config_layers = ConfigLayers::default();
        let client_config = config_layers.resolve_effective_client_config();
        let theme = theme::resolve_theme(&client_config.theme);
        let registry = ActionRegistry::new();
        let keymap_catalog = KeymapHintCatalog::from_registry(&registry);
        Client {
            client_id,
            session_id: None,
            viewport,
            delivery_receiver,
            config_layers,
            client_config,
            theme,
            keymap_catalog,
            registry,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
            pending_key_sequence: None,
            last_mouse_press: None,
            mouse_capture: None,
            resize_drag: None,
            tabline_drag: None,
            tabline_peek: None,
            selection_drag: None,
            selection_scroll_origin_row_index: None,
            hovered_pane_id: None,
            placement_handle_pane_id: None,
            reconnecting: None,
            placement_state: PlacementState::default(),
            active_tab_id: None,
            focused_pane_id: None,
            visible_tab_ids: Vec::new(),
            _terminal_cleanup_guard: cleanup_guard,
        }
    }

    /// Apply the config files this viewer read at startup: `koshi.kdl`'s
    /// viewer-owned sections, the color theme, and `keybinding.kdl`. Each is
    /// `None` when its file is absent or failed to load, and its defaults then
    /// stand — a `None` `keybindings` puts the keymap back on the built-ins
    /// and drops any sequence being typed.
    ///
    /// App settings and colors are typed values and always apply. Keybindings
    /// are all-or-nothing: conflict detection runs over the candidate against
    /// this viewer's action table, and it commits only on a
    /// [`KeymapVerdict::Apply`] — storing the layer, rebuilding the keymap,
    /// and dropping any sequence being typed. A collision puts the built-in
    /// defaults in place with the hint bar's revert marker set; a fatal
    /// finding keeps the running keymap unmarked. Both put the stored layer
    /// and the folded keybinding settings back on the built-ins.
    ///
    /// Returns the conflict report when `keybindings` is `Some`, and `None`
    /// when it is `None`.
    pub fn load_startup_config(
        &mut self,
        app: Option<PartialKoshiConfig>,
        theme: Option<PartialThemeConfig>,
        keybindings: Option<PartialKeybindingsConfig>,
    ) -> Option<ConflictReport> {
        self.config_layers = ConfigLayers::from_files(app.clone(), theme.clone(), None);
        self.client_config = self.config_layers.resolve_effective_client_config();
        self.theme = theme::resolve_theme(&self.client_config.theme);

        let Some(candidate) = keybindings else {
            self.keymap_catalog = KeymapHintCatalog::from_registry(&self.registry);
            self.pending_key_sequence = None;
            return None;
        };
        let user_mode_bindings_by_name = candidate.mode_bindings_by_name.clone();
        let tentative_layers = ConfigLayers::from_files(app, theme, Some(candidate));
        let tentative = tentative_layers.resolve_effective_client_config();
        let key_layers =
            build_keymap_layers(user_mode_bindings_by_name, tentative.keybindings.leader);
        let report = detect_conflicts(
            &key_layers,
            tentative.keybindings.leader,
            tentative.keybindings.unlock_alternative,
            tentative.keybindings.max_chord_depth,
            &self.registry,
        );
        if report.get_verdict() != KeymapVerdict::Apply {
            // A collision reverts to the built-in defaults with the revert
            // marker in the hint bar; a fatal finding keeps the running
            // keymap unmarked.
            if report.get_verdict() == KeymapVerdict::RevertToDefaults {
                self.keymap_catalog =
                    KeymapHintCatalog::from_registry(&self.registry).mark_reverted_to_defaults();
                self.pending_key_sequence = None;
            }
            return Some(report);
        }
        self.config_layers = tentative_layers;
        self.client_config = tentative;
        self.keymap_catalog = KeymapHintCatalog::from_parts(
            &key_layers,
            &self.client_config.keybindings,
            &self.registry,
        );
        // The chords held so far were reaching for bindings the new keymap may
        // not hold, so the sequence is dropped and resolves to nothing.
        self.pending_key_sequence = None;
        Some(report)
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

    /// Record where this viewer's dialing stands, or `None` once it has a link
    /// again. The tabline draws `RECONNECTING (attempt 4, retry in 8s)` for a
    /// `Some(Reconnecting { attempt: 4, retry_in_seconds: 8 })`.
    pub fn set_reconnecting(&mut self, reconnecting: Option<Reconnecting>) {
        self.reconnecting = reconnecting;
    }

    /// The client's own outer-terminal size in cells.
    #[must_use]
    pub fn get_viewport_size(&self) -> Size {
        self.viewport
    }

    /// Record the outer terminal's new size. The caller also reports the
    /// resize to the session, which owns the reconciled tab sizes.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
    }

    /// Record the session that supplies the current event stream.
    pub(crate) fn set_session_id(&mut self, session_id: SessionId) {
        if self.session_id != Some(session_id) {
            self.clear_placement_draft();
        }
        self.session_id = Some(session_id);
    }

    /// Apply the viewer state `render_snapshot` carries, in this order: the
    /// placement revisions, the active tab with its focused pane and tab list,
    /// the lock mode, the tab-strip peek, and whether mouse-select is on. A
    /// tab-strip peek made on another tab is dropped. Each other step can end
    /// pane placement mode or clear its target, as
    /// [`set_placement_revisions`](Self::set_placement_revisions),
    /// [`set_frame_view`](Self::set_frame_view),
    /// [`set_lock_mode`](Self::set_lock_mode), and
    /// [`set_mouse_selection_enabled`](Self::set_mouse_selection_enabled)
    /// describe.
    pub(crate) fn apply_render_snapshot(&mut self, render_snapshot: &RenderSnapshot) {
        self.set_placement_revisions(
            render_snapshot.session_snapshot.session_revision,
            render_snapshot.client_snapshot.client_revision,
        );
        self.set_frame_view(
            render_snapshot.client_snapshot.active_tab_id,
            render_snapshot.client_snapshot.focused_pane_id,
            render_snapshot
                .session_snapshot
                .tabs_metadata
                .iter()
                .map(|tab_metadata| tab_metadata.tab_id)
                .collect(),
        );
        self.set_lock_mode(render_snapshot.client_snapshot.lock_mode);
        self.note_active_tab(render_snapshot.client_snapshot.active_tab_id);
        self.set_mouse_selection_enabled(
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
        );
    }

    /// Copy every field [`apply_render_snapshot`](Self::apply_render_snapshot)
    /// changes. The accepted placement preview is shared, not copied.
    pub(crate) fn build_viewer_restore_point(&self) -> ViewerRestorePoint {
        ViewerRestorePoint {
            lock_mode: self.lock_mode,
            is_mouse_selection_enabled: self.is_mouse_selection_enabled,
            pending_key_sequence: self.pending_key_sequence.clone(),
            tabline_peek: self.tabline_peek,
            active_tab_id: self.active_tab_id,
            focused_pane_id: self.focused_pane_id,
            visible_tab_ids: self.visible_tab_ids.clone(),
            placement_state: self.placement_state.clone(),
        }
    }

    /// Put back every field `viewer_restore_point` copied.
    pub(crate) fn restore_viewer(&mut self, viewer_restore_point: ViewerRestorePoint) {
        let ViewerRestorePoint {
            lock_mode,
            is_mouse_selection_enabled,
            pending_key_sequence,
            tabline_peek,
            active_tab_id,
            focused_pane_id,
            visible_tab_ids,
            placement_state,
        } = viewer_restore_point;
        self.lock_mode = lock_mode;
        self.is_mouse_selection_enabled = is_mouse_selection_enabled;
        self.pending_key_sequence = pending_key_sequence;
        self.tabline_peek = tabline_peek;
        self.active_tab_id = active_tab_id;
        self.focused_pane_id = focused_pane_id;
        self.visible_tab_ids = visible_tab_ids;
        self.placement_state = placement_state;
    }

    /// Record the view carried by the newest painted frame.
    ///
    /// A frame that focuses the placement source pane sets the source tab to
    /// the frame's active tab: after `pane-123` moves from tab `#1` to tab
    /// `#2`, its accepted frame focuses it in `#2`, so closing `#1` leaves pane
    /// placement mode on.
    pub(crate) fn set_frame_view(
        &mut self,
        active_tab_id: TabId,
        focused_pane_id: Option<PaneId>,
        visible_tab_ids: Vec<TabId>,
    ) {
        let has_active_tab_changed = self.active_tab_id != Some(active_tab_id);
        let has_focused_pane_changed = self.focused_pane_id != focused_pane_id;
        let has_visible_tab_list_changed = self.visible_tab_ids != visible_tab_ids;
        let is_reconciling_submitted_placement =
            self.placement_state.needs_placement_reconciliation;
        if is_reconciling_submitted_placement {
            self.placement_state.needs_placement_reconciliation = false;
            let needs_placement_preview_refresh =
                self.placement_state.needs_placement_preview_refresh;
            self.clear_placement_draft();
            self.placement_state.needs_placement_preview_refresh = needs_placement_preview_refresh;
        }
        if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            if focused_pane_id == Some(placement_mode.source_pane_id) {
                placement_mode.source_tab_id = active_tab_id;
            }
        }
        let should_cancel_placement = self.is_placement_mode_active()
            && !is_reconciling_submitted_placement
            && (has_active_tab_changed
                || (has_focused_pane_changed && !self.is_placement_source_pane(focused_pane_id))
                || (has_visible_tab_list_changed
                    && !visible_tab_ids.is_empty()
                    && self.placement_state.placement_mode.as_ref().is_some_and(
                        |placement_mode| !visible_tab_ids.contains(&placement_mode.source_tab_id),
                    )));
        if should_cancel_placement {
            self.cancel_placement_mode();
        }
        self.active_tab_id = Some(active_tab_id);
        self.focused_pane_id = focused_pane_id;
        self.visible_tab_ids = visible_tab_ids;
    }

    /// Record the placement revisions carried by the newest painted frame. When
    /// either revision changed:
    ///
    /// - With a confirmation pending that the session committed, or that was
    ///   pending across a resync or reconnect, this frame shows that
    ///   placement. With `stay-in-pane-placement-mode-after-placement #true`,
    ///   pane placement mode stays on until Esc and reads a fresh preview; a
    ///   mode opened by a pane-handle drag switches to
    ///   [`PlacementModeLifetime::UntilCancelled`]. With `#false`, the mode
    ///   ends.
    /// - With a confirmation pending and no answer yet, the frame shows another
    ///   change, such as a pane another viewer opened. The confirmation stays
    ///   pending until the session commits or rejects it.
    /// - With nothing pending, the unconfirmed target clears, the mode stays
    ///   on, and a drag in progress keeps going: the focus frame that a mouse
    ///   pickup causes does not end the drag that pickup started.
    pub(crate) fn set_placement_revisions(
        &mut self,
        session_placement_revision: u64,
        client_placement_revision: u64,
    ) {
        let has_placement_revision_changed = self.placement_state.session_placement_revision
            != session_placement_revision
            || self.placement_state.client_placement_revision != client_placement_revision;
        let is_placement_confirmation_pending = self.is_placement_confirmation_pending();
        let is_pending_placement_committed = self
            .get_pending_placement_command()
            .is_some_and(|pending_placement_command| pending_placement_command.is_committed);
        let is_waiting_for_placement_answer = is_placement_confirmation_pending
            && !is_pending_placement_committed
            && !self.placement_state.needs_placement_reconciliation;
        let is_placement_mode_active = self.is_placement_mode_active();
        self.placement_state.session_placement_revision = session_placement_revision;
        self.placement_state.client_placement_revision = client_placement_revision;
        if has_placement_revision_changed && !is_waiting_for_placement_answer {
            let placement_drag = self.placement_state.placement_drag;
            let should_stay_in_pane_placement_mode = self
                .client_config
                .should_stay_in_pane_placement_mode_after_placement;
            if is_placement_confirmation_pending && should_stay_in_pane_placement_mode {
                self.placement_state.placement_mode_lifetime =
                    PlacementModeLifetime::UntilCancelled;
            }
            self.clear_placement_draft();
            if is_placement_confirmation_pending
                && is_placement_mode_active
                && !should_stay_in_pane_placement_mode
            {
                self.cancel_placement_mode();
            }
            if !is_placement_confirmation_pending {
                self.placement_state.placement_drag = placement_drag;
            }
            self.placement_state.needs_placement_reconciliation = is_placement_confirmation_pending;
            self.placement_state.needs_placement_preview_refresh = self.is_placement_mode_active();
        }
    }

    /// Enter local placement mode for the focused pane, until Esc. The preview
    /// starts on the active tab: `<C-p> m` on tab `#1` of `[#1, #2]` previews
    /// `#1`. Returns `None` when placement mode is already on, a placement
    /// waits for the session, or nothing is focused.
    pub(crate) fn begin_placement_mode(&mut self) -> Option<(PaneId, TabId)> {
        let source_pane_id = self.focused_pane_id?;
        let source_tab_id = self.active_tab_id?;
        self.open_placement_mode(
            source_pane_id,
            source_tab_id,
            PlacementModeLifetime::UntilCancelled,
        )
    }

    /// Enter local placement mode for `source_pane_id` from a press on its
    /// painted placement handle, until the drag ends. Returns `None` when
    /// placement mode is already on or a placement waits for the session.
    pub(crate) fn begin_mouse_placement_mode(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: TabId,
    ) -> Option<(PaneId, TabId)> {
        self.open_placement_mode(
            source_pane_id,
            source_tab_id,
            PlacementModeLifetime::UntilDragEnds,
        )
    }

    /// Start a placement of `source_pane_id` that previews `source_tab_id`
    /// with no target, drops any open key sequence, tab hover, and preview
    /// read, and returns `(source_pane_id, source_tab_id)`. Returns `None`,
    /// changing nothing, when placement mode is already on or a placement
    /// waits for the session.
    fn open_placement_mode(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: TabId,
        placement_mode_lifetime: PlacementModeLifetime,
    ) -> Option<(PaneId, TabId)> {
        if self.is_placement_mode_active() || self.is_placement_confirmation_pending() {
            return None;
        }
        self.placement_state.placement_mode = Some(PlacementMode {
            source_pane_id,
            source_tab_id,
            destination_tab_id: source_tab_id,
            placement_direction: Direction::Right,
            placement_target: None,
            pending_placement_command: None,
        });
        self.placement_state.placement_mode_lifetime = placement_mode_lifetime;
        self.placement_state.placement_drag = None;
        self.placement_state.placement_tab_hover = None;
        self.pending_key_sequence = None;
        self.clear_placement_read();
        Some((source_pane_id, source_tab_id))
    }

    /// Whether this viewer's keyboard or active mouse drag belongs to placement.
    #[must_use]
    pub(crate) fn is_placement_mode_active(&self) -> bool {
        self.placement_state
            .placement_mode
            .as_ref()
            .is_some_and(|placement_mode| {
                placement_mode.pending_placement_command.is_none()
                    || self.placement_state.placement_mode_lifetime
                        == PlacementModeLifetime::UntilCancelled
            })
    }

    /// Return whether pane placement mode draws its placement view: pane labels
    /// such as `pane-…000000000001`, with no hover tint and no placement handle.
    /// It stays `true` while a submitted placement command waits for the
    /// session's answer.
    #[must_use]
    pub(crate) fn is_pane_placement_visible(&self) -> bool {
        self.is_placement_mode_active() || self.is_placement_confirmation_pending()
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
    pub(crate) fn get_placement_target(&self) -> Option<PanePlacementTarget> {
        self.placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.placement_target.clone())
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
    pub(crate) fn select_placement_destination_tab(
        &mut self,
        destination_tab_id: TabId,
    ) -> Option<(PaneId, TabId)> {
        if self.is_placement_confirmation_pending() {
            return None;
        }
        let source_pane_id = {
            let placement_mode = self.placement_state.placement_mode.as_mut()?;
            if placement_mode.destination_tab_id == destination_tab_id {
                return None;
            }
            placement_mode.destination_tab_id = destination_tab_id;
            placement_mode.placement_target = None;
            placement_mode.source_pane_id
        };
        self.placement_state.placement_drag = None;
        self.placement_state.placement_tab_hover = None;
        self.clear_placement_snapshot();
        Some((source_pane_id, destination_tab_id))
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
                    is_preview_requested: false,
                });
            }
            None => self.placement_state.placement_tab_hover = None,
        }
    }

    /// Return the time until the current placement tab hover may request a preview.
    #[must_use]
    pub(crate) fn next_placement_tab_hover_wakeup(
        &self,
        current_time: Instant,
    ) -> Option<Duration> {
        let placement_tab_hover = self.placement_state.placement_tab_hover?;
        (!placement_tab_hover.is_preview_requested).then(|| {
            placement_tab_hover
                .entered_at
                .checked_add(PLACEMENT_TAB_HOVER_DELAY_DURATION)
                .map_or(Duration::ZERO, |preview_time| {
                    preview_time.saturating_duration_since(current_time)
                })
        })
    }

    /// Return the destination read requested by a completed tab hover.
    pub(crate) fn expire_placement_tab_hover(
        &mut self,
        current_time: Instant,
    ) -> Option<(PaneId, TabId)> {
        let destination_tab_id = {
            let placement_tab_hover = self.placement_state.placement_tab_hover.as_mut()?;
            let preview_time = placement_tab_hover
                .entered_at
                .checked_add(PLACEMENT_TAB_HOVER_DELAY_DURATION)?;
            if placement_tab_hover.is_preview_requested || current_time < preview_time {
                return None;
            }
            placement_tab_hover.is_preview_requested = true;
            placement_tab_hover.tab_id
        };
        self.select_placement_destination_tab(destination_tab_id)
    }

    /// Select `source_pane_id` in `source_tab_id` as the pane to place, without
    /// changing session focus, and return `(source_pane_id, destination_tab_id)`
    /// for its preview read. Returns `None`, changing nothing, while a placement
    /// waits for the session or when `source_pane_id` is already the source
    /// pane, in any tab.
    pub(crate) fn select_placement_source_pane(
        &mut self,
        source_pane_id: PaneId,
        source_tab_id: TabId,
    ) -> Option<(PaneId, TabId)> {
        if self.is_placement_confirmation_pending() {
            return None;
        }
        let destination_tab_id = {
            let placement_mode = self.placement_state.placement_mode.as_mut()?;
            if placement_mode.source_pane_id == source_pane_id {
                return None;
            }
            placement_mode.source_pane_id = source_pane_id;
            placement_mode.source_tab_id = source_tab_id;
            if !self
                .visible_tab_ids
                .contains(&placement_mode.destination_tab_id)
            {
                placement_mode.destination_tab_id = source_tab_id;
            }
            placement_mode.placement_target = None;
            placement_mode.destination_tab_id
        };
        self.placement_state.placement_drag = None;
        self.clear_placement_snapshot();
        Some((source_pane_id, destination_tab_id))
    }

    /// Record the start of a viewer-owned placement drag.
    pub(crate) fn begin_placement_drag(&mut self, start_position: Point, is_insertion_drag: bool) {
        if self.is_placement_mode_active() && !self.is_placement_confirmation_pending() {
            self.placement_state.placement_drag = Some(PlacementDrag {
                start_position,
                is_insertion_drag,
            });
        }
    }

    /// Return whether the current placement drag moved to `position`.
    pub(crate) fn has_placement_drag_moved(&self, position: Point) -> bool {
        self.placement_state
            .placement_drag
            .is_some_and(|placement_drag| placement_drag.start_position != position)
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

    /// Return the newest queued destination once the current read is complete.
    pub(crate) fn take_queued_placement_read(&mut self) -> Option<(PaneId, TabId)> {
        self.placement_state.queued_placement_read.take()
    }

    /// Start one placement preview request, or retain the newest destination while another is pending.
    pub(crate) fn begin_placement_read(
        &mut self,
        request_id: u64,
        source_pane_id: PaneId,
        destination_tab_id: TabId,
        current_time: Instant,
    ) -> bool {
        if self.placement_state.placement_read_request.is_some() {
            self.placement_state.queued_placement_read = Some((source_pane_id, destination_tab_id));
            return false;
        }
        self.placement_state.placement_read_request = Some(PlacementReadRequest {
            request_id,
            source_pane_id,
            destination_tab_id,
            session_placement_revision: self.placement_state.session_placement_revision,
            client_placement_revision: self.placement_state.client_placement_revision,
            expires_at: current_time
                .checked_add(PLACEMENT_READ_TIMEOUT_DURATION)
                .unwrap_or(current_time),
        });
        true
    }

    /// Return the time until the current placement preview request expires.
    #[must_use]
    pub(crate) fn next_placement_read_wakeup(&self, current_time: Instant) -> Option<Duration> {
        self.placement_state
            .placement_read_request
            .as_ref()
            .map(|placement_read_request| {
                placement_read_request
                    .expires_at
                    .saturating_duration_since(current_time)
            })
    }

    /// Return whether one placement preview request is still in flight.
    #[must_use]
    pub(crate) fn is_placement_read_pending(&self) -> bool {
        self.placement_state.placement_read_request.is_some()
    }

    /// Whether the placement status reads `loading`: a preview read is in
    /// flight, or the end of this attachment loop pass sends one.
    #[must_use]
    pub(crate) fn is_placement_preview_loading(&self) -> bool {
        self.is_placement_read_pending() || self.find_placement_preview_refresh().is_some()
    }

    /// Expire the current placement preview request when its deadline has passed.
    pub(crate) fn expire_placement_read(&mut self, current_time: Instant) -> bool {
        let is_expired = self
            .placement_state
            .placement_read_request
            .as_ref()
            .is_some_and(|placement_read_request| {
                placement_read_request.expires_at <= current_time
            });
        if is_expired {
            self.placement_state.placement_read_request = None;
        }
        is_expired
    }

    /// Accept a placement preview only when it answers the current request and
    /// still describes the revisions that were current when it was sent. An
    /// accepted preview of another tab with no target selected and no drag
    /// active selects insertion at the right edge of that whole tab. An
    /// accepted preview also sets the source tab to the one it names.
    pub(crate) fn accept_placement_snapshot(
        &mut self,
        request_id: u64,
        placement_snapshot: PanePlacementSnapshot,
        current_time: Instant,
    ) -> bool {
        let Some(placement_read_request) = self.placement_state.placement_read_request else {
            return false;
        };
        if placement_read_request.request_id != request_id {
            return false;
        }
        if placement_read_request.expires_at <= current_time {
            self.placement_state.placement_read_request = None;
            return false;
        }
        self.placement_state.placement_read_request = None;
        let is_current_placement_request =
            self.placement_state
                .placement_mode
                .as_ref()
                .is_none_or(|placement_mode| {
                    placement_mode.source_pane_id == placement_read_request.source_pane_id
                        && placement_mode.destination_tab_id
                            == placement_read_request.destination_tab_id
                });
        if !is_current_placement_request
            || placement_snapshot.source_pane_id != placement_read_request.source_pane_id
            || placement_snapshot.destination_tab_id != placement_read_request.destination_tab_id
            || placement_snapshot.session_placement_revision
                != placement_read_request.session_placement_revision
            || placement_snapshot.client_placement_revision
                != placement_read_request.client_placement_revision
            || self.session_id != Some(placement_snapshot.session_id)
            || placement_snapshot.client_snapshot.client_id != self.client_id
            || placement_snapshot.validate().is_err()
        {
            return false;
        }
        if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            placement_mode.source_tab_id = placement_snapshot.source_tab_id;
        }
        self.placement_state.placement_snapshot = Some(Arc::new(placement_snapshot));
        self.select_whole_tab_insertion_target();
        true
    }

    /// Consume a refusal for the current placement preview request.
    ///
    /// A missing source or destination ends the local placement interaction.
    /// A resource-limit refusal leaves the interaction available for another
    /// request.
    pub(crate) fn accept_placement_refusal(
        &mut self,
        request_id: u64,
        placement_error_code: IpcErrorCode,
        current_time: Instant,
    ) -> bool {
        let Some(placement_read_request) = self.placement_state.placement_read_request else {
            return false;
        };
        if placement_read_request.request_id != request_id {
            return false;
        }
        self.placement_state.placement_read_request = None;
        if placement_read_request.expires_at <= current_time {
            return false;
        }
        let is_current_placement_request =
            self.placement_state
                .placement_mode
                .as_ref()
                .is_none_or(|placement_mode| {
                    placement_mode.source_pane_id == placement_read_request.source_pane_id
                        && placement_mode.destination_tab_id
                            == placement_read_request.destination_tab_id
                });
        if placement_error_code == IpcErrorCode::NotFound
            && is_current_placement_request
            && self.is_placement_mode_active()
        {
            self.cancel_placement_mode();
        }
        true
    }

    /// Drop the retained placement preview while keeping a request in flight.
    pub(crate) fn clear_placement_snapshot(&mut self) {
        self.placement_state.placement_snapshot = None;
    }

    /// Drop the preview read, the queued read, and the drag. With no placement
    /// command pending, also clear the placement draft.
    pub(crate) fn clear_placement_read(&mut self) {
        self.placement_state.placement_read_request = None;
        self.placement_state.queued_placement_read = None;
        self.placement_state.placement_drag = None;
        if !self.is_placement_confirmation_pending() {
            self.clear_placement_draft();
        }
    }

    /// End pane placement mode and drop its preview, tab hover, open key
    /// sequence, and preview reads. Does nothing while a placement command
    /// waits for the session.
    pub(crate) fn cancel_placement_mode(&mut self) {
        if self.is_placement_confirmation_pending() {
            return;
        }
        self.placement_state.placement_mode = None;
        self.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
        self.placement_state.placement_tab_hover = None;
        self.pending_key_sequence = None;
        self.clear_placement_read();
    }

    /// Drop the preview reads, the preview, the tab hover, the drag, and any due
    /// preview refresh or reconciliation. A mode that lasts until its drag ends
    /// closes when a placement command is pending. Any other mode stays on with
    /// no target and no pending placement command.
    pub(crate) fn clear_placement_draft(&mut self) {
        self.placement_state.needs_placement_reconciliation = false;
        self.placement_state.needs_placement_preview_refresh = false;
        self.placement_state.placement_read_request = None;
        self.placement_state.queued_placement_read = None;
        self.placement_state.placement_drag = None;
        self.placement_state.placement_tab_hover = None;
        self.clear_placement_snapshot();
        let is_drag_confirmation_pending = self.is_placement_confirmation_pending()
            && self.placement_state.placement_mode_lifetime == PlacementModeLifetime::UntilDragEnds;
        if is_drag_confirmation_pending {
            self.placement_state.placement_mode = None;
            self.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
        } else if let Some(placement_mode) = self.placement_state.placement_mode.as_mut() {
            placement_mode.placement_target = None;
            placement_mode.pending_placement_command = None;
        }
    }

    /// Return the placement command this client sent and the session has not
    /// yet answered, or `None` when no confirmation waits.
    fn get_pending_placement_command(&self) -> Option<PendingPlacementCommand> {
        self.placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.pending_placement_command)
    }

    /// Return whether `command_id` is the placement command this client sent
    /// and the session has not yet answered.
    fn is_pending_placement_command(&self, command_id: CommandId) -> bool {
        self.get_pending_placement_command()
            .is_some_and(|pending_placement_command| {
                pending_placement_command.command_id == command_id
            })
    }

    /// Record that the session committed placement command `command_id`, and
    /// return `true`. Returns `false`, changing nothing, when `command_id` is
    /// not the pending placement command, such as another viewer's placement.
    pub(crate) fn note_placement_command_committed(&mut self, command_id: CommandId) -> bool {
        let Some(pending_placement_command) = self
            .placement_state
            .placement_mode
            .as_mut()
            .and_then(|placement_mode| placement_mode.pending_placement_command.as_mut())
        else {
            return false;
        };
        if pending_placement_command.command_id != command_id {
            return false;
        }
        pending_placement_command.is_committed = true;
        true
    }

    /// Clear the pending placement confirmation that the session rejected, and
    /// return `true`. Pane placement mode that lasts until Esc stays on and
    /// reads a fresh preview; a mode that lasts until its drag ends closes.
    /// Returns `false`, changing nothing, when `command_id` is not the pending
    /// placement command.
    pub(crate) fn reject_placement_command(&mut self, command_id: CommandId) -> bool {
        if !self.is_pending_placement_command(command_id) {
            return false;
        }
        let should_refresh_placement_preview =
            self.placement_state.placement_mode_lifetime == PlacementModeLifetime::UntilCancelled;
        self.clear_placement_draft();
        if should_refresh_placement_preview && self.is_placement_mode_active() {
            self.placement_state.needs_placement_preview_refresh = true;
        }
        true
    }

    /// Whether a placement command awaits the session's answer. Releasing over
    /// `pane-123` keeps this true until the session rejects the command, or
    /// commits it and the frame that shows it arrives.
    #[must_use]
    pub(crate) fn is_placement_confirmation_pending(&self) -> bool {
        self.get_pending_placement_command().is_some()
    }

    /// Mark a submitted placement for reconciliation with the next frame.
    pub(crate) fn prepare_placement_reconciliation(&mut self) {
        let is_placement_confirmation_pending = self.is_placement_confirmation_pending();
        let is_placement_mode_until_cancelled =
            self.placement_state.placement_mode_lifetime == PlacementModeLifetime::UntilCancelled;
        self.placement_state.placement_read_request = None;
        self.placement_state.queued_placement_read = None;
        self.placement_state.placement_drag = None;
        self.clear_placement_snapshot();
        self.placement_state.needs_placement_reconciliation = is_placement_confirmation_pending;
        self.placement_state.needs_placement_preview_refresh =
            is_placement_confirmation_pending && is_placement_mode_until_cancelled;
        if !is_placement_confirmation_pending {
            self.clear_placement_draft();
        }
    }

    /// Return the source pane and destination tab of the fresh preview read
    /// that the end of this attachment loop pass sends. Returns `None` when no
    /// refresh is due, pane placement mode is off, a placement command waits
    /// for the session, or a preview read is in flight.
    fn find_placement_preview_refresh(&self) -> Option<(PaneId, TabId)> {
        if !self.placement_state.needs_placement_preview_refresh
            || !self.is_placement_mode_active()
            || self.is_placement_confirmation_pending()
            || self.placement_state.placement_read_request.is_some()
        {
            return None;
        }
        let placement_mode = self.placement_state.placement_mode.as_ref()?;
        Some((
            placement_mode.source_pane_id,
            placement_mode.destination_tab_id,
        ))
    }

    /// Return the preview read [`find_placement_preview_refresh`] names and
    /// clear the refresh, whether or not a read is returned.
    ///
    /// [`find_placement_preview_refresh`]: Self::find_placement_preview_refresh
    pub(crate) fn take_placement_preview_refresh(&mut self) -> Option<(PaneId, TabId)> {
        let placement_preview_refresh = self.find_placement_preview_refresh();
        self.placement_state.needs_placement_preview_refresh = false;
        placement_preview_refresh
    }

    /// The newest accepted placement preview.
    #[must_use]
    pub fn get_placement_snapshot(&self) -> Option<&PanePlacementSnapshot> {
        self.placement_state.placement_snapshot.as_deref()
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
    /// reads.
    pub fn set_mouse_selection_enabled(&mut self, is_mouse_selection_enabled: bool) {
        if self.is_mouse_selection_enabled != is_mouse_selection_enabled
            && self.is_placement_mode_active()
        {
            self.cancel_placement_mode();
        }
        self.is_mouse_selection_enabled = is_mouse_selection_enabled;
    }

    /// The hint-bar data one frame is painted from, using `mode` and the
    /// acting client's mouse-selection state.
    ///
    /// The entry labelled [`MOUSE_SELECT_HINT`] reads [`MOUSE_UNSELECT_HINT`]
    /// while mouse-selection mode is enabled.
    #[must_use]
    pub(crate) fn build_frame_hints(
        &self,
        lock_mode: LockMode,
        is_mouse_selection_enabled: bool,
    ) -> KeymapHints {
        mouse_select_hints(
            self.keymap_catalog.build_hints_for_mode(lock_mode),
            is_mouse_selection_enabled,
        )
    }

    /// Take everything the subscription has delivered and apply what the
    /// viewer must react to, returning how many deliveries were seen.
    ///
    /// The events that matter are the session's reports that this
    /// viewer's input mode changed — which happens when `koshi lock --client`
    /// names it, or when its own lock binding fires — and that its mouse-select
    /// mode changed, which happens when its own `core:mouse-select` binding
    /// fires. Both decide what an input means before anything is sent, and
    /// applying them here keeps the viewer's copies agreeing with the
    /// session's. An event naming another client is skipped.
    ///
    /// A fresh frame arrives when this subscriber's queue overflowed and the
    /// session dropped events it cannot replay. It carries the session's own
    /// copies of both, and the tab this viewer is on. The viewer takes all
    /// three from the frame — a tab-strip peek made on another tab is thrown
    /// away with the last of them — and logs how many events were dropped. The
    /// frame names this viewer, checked by a debug assertion.
    ///
    /// A [`Delivery::Frame`] is the picture composed for a client in another
    /// process. It is counted, and nothing is taken from it. So is a
    /// [`Delivery::MouseAnswer`], which answers that client's mouse round, a
    /// [`Delivery::HostWrite`], which that client writes to its own terminal,
    /// and a [`Delivery::SwitchTo`], which moves that client to another
    /// session. A [`Delivery::PlacementCommandRejected`] clears the matching
    /// pending placement command.
    pub fn apply_events(&mut self) -> usize {
        let mut delivery_count = 0;
        while let Ok(delivery) = self.delivery_receiver.try_recv() {
            delivery_count += 1;
            match delivery {
                Delivery::Event(event) => match &event {
                    Event::InputModeChanged(mode_change)
                        if mode_change.client_id == self.client_id =>
                    {
                        self.set_lock_mode(mode_change.lock_mode);
                    }
                    Event::MouseSelectChanged(mouse_selection_change)
                        if mouse_selection_change.client_id == self.client_id =>
                    {
                        self.set_mouse_selection_enabled(mouse_selection_change.is_enabled);
                    }
                    _ => {}
                },
                // A frame, a mouse round's answers, terminal bytes, and a
                // session move all belong to a client in another process, which
                // reads them off its own connection.
                Delivery::Frame(_)
                | Delivery::PanePlacementSnapshot { .. }
                | Delivery::PanePlacementRefused { .. }
                | Delivery::MouseAnswer { .. }
                | Delivery::HostWrite(_)
                | Delivery::SwitchTo(_) => {}
                Delivery::PlacementCommandRejected(command_id) => {
                    self.reject_placement_command(command_id);
                }
                Delivery::Snapshot {
                    render_snapshot,
                    lag_report,
                } => {
                    debug_assert_eq!(
                        render_snapshot.client_snapshot.client_id, self.client_id,
                        "a frame names the client its subscriber views"
                    );
                    tracing::warn!(
                        dropped_event_count = lag_report.dropped_event_count,
                        "events were dropped; resuming from a fresh frame"
                    );
                    self.apply_render_snapshot(&render_snapshot);
                    self.clear_placement_read();
                }
            }
        }
        delivery_count
    }
}

/// `hints` with the `core:mouse-select` entry wearing its "on" label, so the
/// hint bar reads `Mouse Unselect` while mouse-select mode is active.
///
/// `on` false returns `hints` untouched. `on` true returns a copy in which
/// every entry labelled [`MOUSE_SELECT_HINT`] is relabelled
/// [`MOUSE_UNSELECT_HINT`]; nothing else changes. Matching is on the label, so
/// a rebound or duplicated binding flips too.
fn mouse_select_hints(keymap_hints: KeymapHints, is_mouse_selection_enabled: bool) -> KeymapHints {
    if !is_mouse_selection_enabled {
        return keymap_hints;
    }
    let hint_bindings: Vec<HintBinding> = keymap_hints
        .hint_bindings
        .iter()
        .map(|hint_binding| {
            let mut hint_binding = hint_binding.clone();
            if hint_binding.action_display_name == MOUSE_SELECT_HINT {
                MOUSE_UNSELECT_HINT.clone_into(&mut hint_binding.action_display_name);
            }
            hint_binding
        })
        .collect();
    KeymapHints {
        hint_bindings: Arc::new(hint_bindings),
        ..keymap_hints
    }
}
