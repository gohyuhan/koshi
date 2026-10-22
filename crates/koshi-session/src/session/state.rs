//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use koshi_core::{
    constant::{
        FLOATING_PANE_CHROME_SIZE, MAX_FLOATING_PANES_PER_SESSION, MAX_TAB_FOCUS_MRU_ENTRY_COUNT,
    },
    event::Event,
    geometry::{
        compute_percent_cell_count, FloatingPaneDimension, FloatingPaneSize, PixelCellSize, Rect,
        Size,
    },
    ids::{ClientId, PaneId, SessionId, TabId},
};
use koshi_layout::{
    mode::LayoutMode,
    solver::{solve_layout_with_mode, solve_layout_with_sizing, LayoutSolve, PaneSizing},
    tree::LayoutNode,
};
use koshi_pane::registry::PaneRegistry;
use serde::{Deserialize, Serialize};

use crate::{
    client::{
        compute_floating_pane_size_cap, place_floating_pane, Client, ClientRegistry,
        FloatingPanePosition,
    },
    error::{FloatingSetError, InvalidTransition, SessionConsistencyError},
    session::cascade::hand_off_suppressed_focus,
    session::focus::{build_pane_focused_event, build_pane_minimized_changed_events},
    session::lifecycle::{SessionLifecycle, SessionLifecycleEvent},
};

/// One tab: its name, bar position, layout tree, the tiled solve of that
/// tree, and the panes it focused, most-recent first.
///
/// A tab holds no layout mode. Zoom is a client property: it lives on
/// [`crate::client::Client`] as `zoomed_pane_id_by_tab_id`, and two clients on this tab can
/// hold different zoom. The tab holds the tree and its tiled solve, and each
/// client's own view comes from that solve ([`Session::compute_client_solve`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    tab_id: TabId,
    tab_name: String,
    tab_index: usize,
    layout: LayoutNode,
    /// Panes this tab has focused, most-recent first, with at most one entry
    /// per pane — re-focusing moves a pane to the front instead of adding a
    /// duplicate. Capped at [`MAX_TAB_FOCUS_MRU_ENTRY_COUNT`]; focus recovery walks
    /// it newest-first to pick the inheriting pane when the focused one
    /// disappears.
    focus_mru: Vec<PaneId>,
    /// The tab size `layout_solve` is solved at.
    solved_tab_size: Size,
    /// The tiled solve of `layout` over a `solved_tab_size` rect at `(0, 0)`
    /// with the session's pane sizing ([`Session::get_pane_sizing`]). The
    /// resume file does not carry it: a tab read from one holds the solve of
    /// no pane until [`Session::update_pane_sizing`] solves it again.
    #[serde(skip)]
    layout_solve: LayoutSolve,
}

impl Tab {
    /// A tab holding `layout_tree`, with no focus recorded yet, solved at
    /// `tab_size` with `pane_sizing`.
    #[must_use]
    pub fn from_layout_tree(
        tab_id: TabId,
        tab_name: String,
        tab_index: usize,
        layout_tree: LayoutNode,
        tab_size: Size,
        pane_sizing: PaneSizing,
    ) -> Self {
        let mut tab = Self {
            tab_id,
            tab_name,
            tab_index,
            layout: layout_tree,
            focus_mru: Vec::new(),
            solved_tab_size: tab_size,
            layout_solve: LayoutSolve::default(),
        };
        let _ = tab.update_layout_solve(pane_sizing);
        tab
    }

    /// This tab's stable id, matching its key in [`Session::tabs`].
    #[must_use]
    pub fn get_tab_id(&self) -> TabId {
        self.tab_id
    }

    /// The name shown for this tab in the tab bar.
    #[must_use]
    pub fn get_tab_name(&self) -> &str {
        &self.tab_name
    }

    /// This tab's display position in the bar; kept a dense `0..n` across the
    /// session's tabs by the tab operations.
    #[must_use]
    pub fn get_tab_index(&self) -> usize {
        self.tab_index
    }

    /// This tab's layout tree.
    #[must_use]
    pub fn get_layout_tree(&self) -> &LayoutNode {
        &self.layout
    }

    /// Set this tab's display position. Callers keep positions a dense `0..n`
    /// across the session's tabs.
    pub fn update_tab_index(&mut self, tab_index: usize) {
        self.tab_index = tab_index;
    }

    /// The tiled solve of this tab's tree at [`Tab::get_solved_tab_size`].
    #[must_use]
    pub fn get_layout_solve(&self) -> &LayoutSolve {
        &self.layout_solve
    }

    /// The tab size this tab's tree is solved at.
    #[must_use]
    pub fn get_solved_tab_size(&self) -> Size {
        self.solved_tab_size
    }

    /// Solve this tab's tree over a `solved_tab_size` rect at `(0, 0)` with
    /// `pane_sizing` ([`solve_layout_with_sizing`]), and hold that solve.
    /// Returns the solve the tab held before.
    fn update_layout_solve(&mut self, pane_sizing: PaneSizing) -> LayoutSolve {
        let layout_solve = solve_layout_with_sizing(
            &self.layout,
            Rect::from_size_at_origin(self.solved_tab_size),
            pane_sizing,
        );
        std::mem::replace(&mut self.layout_solve, layout_solve)
    }

    /// Records `pane_id` as the most-recently focused: moves it to the front,
    /// keeping one entry per pane, then cuts the history back to
    /// [`MAX_TAB_FOCUS_MRU_ENTRY_COUNT`] entries, dropping the oldest.
    ///
    /// A history restored longer than the cap — a session file this process did
    /// not write — is cut to the cap by this one call, not by one entry.
    pub fn record_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru
            .retain(|&focused_pane_id| focused_pane_id != pane_id);
        self.focus_mru.insert(0, pane_id);
        self.focus_mru
            .truncate(usize::from(MAX_TAB_FOCUS_MRU_ENTRY_COUNT));
    }

    /// The panes this tab has focused, most-recent first.
    pub fn list_focus_mru(&self) -> &[PaneId] {
        &self.focus_mru
    }

    /// Remove `pane_id` from this tab's focus history.
    pub fn remove_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru
            .retain(|&focused_pane_id| focused_pane_id != pane_id);
    }
}

/// One floating pane: the pane, the size it asks for, and the size it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingMember {
    /// The pane: a record in [`Session::panes`] that no tab's layout holds as
    /// a leaf.
    pub pane_id: PaneId,
    /// The size the pane asks for, per axis.
    pub desired_size: FloatingPaneSize,
    /// The pane's outer size in cells from its last size solve, chrome
    /// included. Its PTY is this size less [`FLOATING_PANE_CHROME_SIZE`].
    pub solved_size: Size,
}

/// The floating panes of one session, in the order the panes became floating:
/// a new member is appended, and a removal keeps the order of the rest.
///
/// The set holds each pane once and at most
/// [`MAX_FLOATING_PANES_PER_SESSION`] members, which
/// [`Session::add_floating_member`] adds.
/// [`FloatingSet::update_member_sizes`] writes each member's solved size.
/// [`Session::remove_floating_member`] removes a member and every client's
/// view of it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingSet {
    members: Vec<FloatingMember>,
}

impl FloatingSet {
    /// Every member, in the order the panes became floating.
    #[must_use]
    pub fn list_members(&self) -> &[FloatingMember] {
        &self.members
    }

    /// Every member's pane id, in the order the panes became floating.
    #[must_use]
    pub fn list_pane_ids(&self) -> Vec<PaneId> {
        self.members
            .iter()
            .map(|floating_member| floating_member.pane_id)
            .collect()
    }

    /// The member holding `pane_id`, with its index in the order the panes
    /// became floating: its cascade index ([`place_floating_pane`]).
    /// `None` when no member holds `pane_id`. With members `[a, b]`, `b` →
    /// `Some((1, b))`.
    #[must_use]
    pub fn find_member(&self, pane_id: PaneId) -> Option<(usize, FloatingMember)> {
        self.members
            .iter()
            .copied()
            .enumerate()
            .find(|(_, floating_member)| floating_member.pane_id == pane_id)
    }

    /// Whether a member holds `pane_id`.
    #[must_use]
    pub fn has_pane(&self, pane_id: PaneId) -> bool {
        self.find_member(pane_id).is_some()
    }

    /// Whether the set holds [`MAX_FLOATING_PANES_PER_SESSION`] members or
    /// more, so [`Session::add_floating_member`] refuses a new one.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.members.len() >= MAX_FLOATING_PANES_PER_SESSION
    }

    /// Append `floating_member` as the newest member.
    ///
    /// # Errors
    ///
    /// [`FloatingSetError::DuplicatePane`] when the set already holds
    /// `floating_member.pane_id`, else [`FloatingSetError::TooManyPanes`] when
    /// [`FloatingSet::is_full`]. The set does not change on an error.
    pub(crate) fn add_member(
        &mut self,
        floating_member: FloatingMember,
    ) -> Result<(), FloatingSetError> {
        if self.has_pane(floating_member.pane_id) {
            return Err(FloatingSetError::DuplicatePane {
                pane_id: floating_member.pane_id,
            });
        }
        if self.is_full() {
            return Err(FloatingSetError::TooManyPanes);
        }
        self.members.push(floating_member);
        Ok(())
    }

    /// Solve every member's outer size against `shared_floating_viewport`
    /// with [`solve_floating_pane_size`]. Writes only `solved_size`.
    pub fn update_member_sizes(&mut self, shared_floating_viewport: Size, pane_minimum_size: Size) {
        for floating_member in &mut self.members {
            floating_member.solved_size = solve_floating_pane_size(
                floating_member.desired_size,
                shared_floating_viewport,
                pane_minimum_size,
            );
        }
    }

    /// Set the desired size of the member holding `pane_id`. Its solved size
    /// changes at the next [`FloatingSet::update_member_sizes`]. Returns
    /// `false`, changing nothing, when no member holds `pane_id`.
    pub fn update_member_desired_size(
        &mut self,
        pane_id: PaneId,
        desired_size: FloatingPaneSize,
    ) -> bool {
        let Some((member_index, _)) = self.find_member(pane_id) else {
            return false;
        };
        self.members[member_index].desired_size = desired_size;
        true
    }
}

/// The floating minimum: `pane_minimum_size` plus
/// [`FLOATING_PANE_CHROME_SIZE`], each axis capped at `u16::MAX`. `20x6` →
/// `22x10`; `65534x6` → `65535x10`.
#[must_use]
pub fn compute_floating_pane_minimum_size(pane_minimum_size: Size) -> Size {
    Size {
        column_count: pane_minimum_size
            .column_count
            .saturating_add(FLOATING_PANE_CHROME_SIZE.column_count),
        row_count: pane_minimum_size
            .row_count
            .saturating_add(FLOATING_PANE_CHROME_SIZE.row_count),
    }
}

/// Solve one floating pane's outer size: `desired_size` against
/// `shared_floating_viewport`, with `pane_minimum_size` as the smallest content
/// size the pane asks for.
///
/// Each axis of `desired_size` resolves to cells against the same axis of
/// `shared_floating_viewport`, a percent rounding down, then is raised to the
/// floating minimum ([`compute_floating_pane_minimum_size`]) and cut to the
/// floating cap of that axis ([`compute_floating_pane_size_cap`], 90% rounded
/// down). On an axis whose cap is shorter than the floating minimum, the pane
/// takes the cap. `Percent(60)` by `Percent(60)` on `80x22` → `48x13`;
/// `Percent(100)` by `Percent(100)` on `80x22` → `72x19`; with a `20x6` pane
/// minimum on `40x9` → `24x8`.
#[must_use]
pub fn solve_floating_pane_size(
    desired_size: FloatingPaneSize,
    shared_floating_viewport: Size,
    pane_minimum_size: Size,
) -> Size {
    let floating_pane_minimum_size = compute_floating_pane_minimum_size(pane_minimum_size);
    let floating_pane_size_cap = compute_floating_pane_size_cap(shared_floating_viewport);
    Size {
        column_count: solve_floating_pane_axis(
            desired_size.width,
            shared_floating_viewport.column_count,
            floating_pane_minimum_size.column_count,
            floating_pane_size_cap.column_count,
        ),
        row_count: solve_floating_pane_axis(
            desired_size.height,
            shared_floating_viewport.row_count,
            floating_pane_minimum_size.row_count,
            floating_pane_size_cap.row_count,
        ),
    }
}

/// The cells `floating_pane_dimension` takes on an axis `axis_cell_count`
/// cells long, given a floating minimum of `minimum_cell_count` and a cap of
/// `cap_cell_count` on that axis: the cell count, or the percent of the axis
/// rounded down, raised to `minimum_cell_count`, then cut to
/// `cap_cell_count`. `Percent(60)` of `22` with a minimum of `5` and a cap of
/// `19` → `13`; `Percent(60)` of `9` with a minimum of `10` and a cap of `8` →
/// `8`.
fn solve_floating_pane_axis(
    floating_pane_dimension: FloatingPaneDimension,
    axis_cell_count: u16,
    minimum_cell_count: u16,
    cap_cell_count: u16,
) -> u16 {
    let requested_cell_count = match floating_pane_dimension {
        FloatingPaneDimension::Cells(cell_count) => cell_count.get(),
        FloatingPaneDimension::Percent(axis_percent) => {
            compute_percent_cell_count(axis_cell_count, axis_percent.get_percent())
        }
    };
    requested_cell_count
        .max(minimum_cell_count)
        .min(cap_cell_count)
}

/// One running session: the aggregate root owning the tabs, the floating
/// panes, the pane registry, and the attached-client registry.
///
/// Anything one client may see differently from another — focus, viewport,
/// input mode — lives on that client's entry in [`ClientRegistry`], never as a
/// session-global field. Two attached clients can look at different tabs and
/// panes at the same time. `should_start_locked` is the mode the session hands the
/// first client to attach, not a mode the session is in.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    /// Unique id, stable for the session's whole life.
    pub session_id: SessionId,
    /// Human-facing name; attach and list address sessions by it.
    pub session_name: String,
    /// When the session was created. Supplied by the caller at the creation
    /// boundary, never read from the clock here.
    pub created_at: SystemTime,
    /// The session's tabs, keyed by id. Display order is not the map order: it
    /// lives on each tab as its display index, and reordering tabs moves no map
    /// entry.
    pub tabs: BTreeMap<TabId, Tab>,
    /// The session's floating panes, in the order the panes became floating.
    /// Each attached client holds its own view of each one.
    pub floating_set: FloatingSet,
    /// Runtime metadata for every pane in every tab and every floating pane.
    /// Layout trees and the floating set name each pane by its id.
    pub panes: PaneRegistry,
    /// The clients currently attached.
    pub clients: ClientRegistry,
    /// True while the next client to attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked). A profile
    /// carrying the `lock` marker sets it; [`Session::take_start_lock`] reads
    /// it and clears it: exactly one attach is locked. A session seeded
    /// without that marker holds `false` and locks nobody.
    pub should_start_locked: bool,

    /// A restart seeded a new shell after the carried session could not be
    /// restored. The statusline shows the notice until input reaches a pane.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_recovery_notice_visible: bool,

    /// Generation of committed layout, membership, and shared sizing inputs.
    placement_revision: u64,

    /// The pane sizing every tab is solved with ([`Session::get_pane_sizing`]).
    /// The resume file does not carry it.
    #[serde(skip)]
    pane_sizing: PaneSizing,

    lifecycle: SessionLifecycle,
}

impl Session {
    /// A session with no tabs and no panes, holding the supplied client
    /// registry. Starts in `Starting` with `should_start_locked`
    /// `false`. `created_at` is supplied by the caller at the creation
    /// boundary, never read from the clock here.
    #[must_use]
    pub fn from_identity_and_client_registry(
        session_id: SessionId,
        session_name: String,
        created_at: SystemTime,
        client_registry: ClientRegistry,
    ) -> Self {
        Self {
            session_id,
            session_name,
            created_at,
            tabs: BTreeMap::new(),
            floating_set: FloatingSet::default(),
            panes: PaneRegistry::new(),
            clients: client_registry,
            should_start_locked: false,
            is_recovery_notice_visible: false,
            placement_revision: 0,
            pane_sizing: PaneSizing::default(),
            lifecycle: SessionLifecycle::Starting,
        }
    }

    /// Whether this attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), clearing the
    /// flag as it reads it.
    ///
    /// Reads [`should_start_locked`](Self::should_start_locked) and clears it in one step:
    /// it returns `true` at most once per session.
    pub fn take_start_lock(&mut self) -> bool {
        std::mem::take(&mut self.should_start_locked)
    }

    /// The session's current lifecycle state.
    pub fn get_lifecycle(&self) -> &SessionLifecycle {
        &self.lifecycle
    }

    /// The generation of this session's committed placement inputs.
    #[must_use]
    pub fn get_placement_revision(&self) -> u64 {
        self.placement_revision
    }

    /// Whether the session revision can advance without wrapping.
    #[must_use]
    pub fn can_advance_placement_revision(&self) -> bool {
        self.placement_revision < u64::MAX
    }

    /// Advance the session revision once. Returns `false` at the maximum value.
    pub fn advance_placement_revision(&mut self) -> bool {
        let Some(next_revision) = self.placement_revision.checked_add(1) else {
            return false;
        };
        self.placement_revision = next_revision;
        true
    }

    /// Apply a `lifecycle_event`, advancing the session's state, or return
    /// [`InvalidTransition`] if the move is illegal from the current state.
    /// Crate-internal: callers drive the lifecycle through the typed wrappers
    /// ([`Session::attach_client`], [`Session::detach_client`],
    /// [`Session::request_session_stop`], [`Session::complete_session_stop`]) or the tab
    /// operations. Each caller
    /// decides whether a rejected event is an expected no-op to ignore (a
    /// re-attach to an already-`Running` session) or a fault to abort on (a tab
    /// created under a wound-down session).
    pub(crate) fn update_lifecycle(
        &mut self,
        lifecycle_event: SessionLifecycleEvent,
    ) -> Result<(), InvalidTransition> {
        self.lifecycle = self.lifecycle.transition(lifecycle_event)?;
        Ok(())
    }

    /// Attach `client` and mark the session live, returning the record it
    /// displaced when that id was already attached (a re-attach replaces in
    /// place), else `None`. `ClientAttached` moves a `Detaching` (no-client)
    /// session to `Running`; from `Starting`, `Running`, `Stopping` or
    /// `Stopped` it is rejected and the lifecycle stays as it was. The client
    /// is registered either way.
    ///
    /// Before it is registered, `client` takes a minimized view at the default
    /// placement of every floating pane it holds no view of
    /// ([`Client::add_floating_pane_view`]): an attach shows no floating pane.
    pub fn attach_client(&mut self, mut client: Client) -> Option<Client> {
        for floating_member in self.floating_set.list_members() {
            client.add_floating_pane_view(floating_member.pane_id, FloatingPanePosition::Default);
        }
        let displaced_client = self.clients.attach_client(client);
        let _ = self.update_lifecycle(SessionLifecycleEvent::ClientAttached);
        displaced_client
    }

    /// Detach the client `client_id`, returning the removed record (`None` if it
    /// was not attached). When it was the *last* attached client the session
    /// drops to `Detaching` — its tabs and panes stay alive; detaching one of
    /// several clients leaves the session `Running`.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        let detached_client = self.clients.detach_client(client_id);
        if !self.clients.has_clients() {
            // Only a `Running` session moves to `Detaching`. `Starting`,
            // `Detaching`, `Stopping` and `Stopped` reject the event and keep
            // the state they had.
            let _ = self.update_lifecycle(SessionLifecycleEvent::LastClientDetached);
        }
        detached_client
    }

    /// The tab whose layout holds `pane_id` as a leaf. `None` when no tab
    /// does: a floating pane, or a pane this session does not hold.
    #[must_use]
    pub fn find_tab_by_pane_id(&self, pane_id: PaneId) -> Option<&Tab> {
        self.tabs.values().find(|tab| tab.layout.has_pane(pane_id))
    }

    /// The pane region to size tab `tab_id` against: each viewing client's own
    /// pane area, reduced to the per-axis minimum (`column_count` and `row_count`
    /// independently), which is the largest grid that fits inside *every*
    /// viewer on *both* axes.
    ///
    /// Every attached client whose [`Client::get_active_tab_id`] is `tab_id`
    /// contributes its [`Client::get_pane_area`]; a viewer that reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
    /// contributes nothing. Returns `None` when no viewer of `tab_id`
    /// contributes a size. The result does not depend on which client (if any)
    /// issued the command, nor on the order the viewers attached.
    #[must_use]
    pub fn get_tab_size(&self, tab_id: TabId) -> Option<Size> {
        self.clients
            .list_attached_clients()
            .filter(|client| client.get_active_tab_id() == tab_id)
            .filter_map(Client::get_pane_area)
            .reduce(Size::compute_minimum_axes)
    }

    /// The pane sizing every tab of this session is solved with.
    /// [`PaneSizing::default`] until [`Session::update_pane_sizing`] sets it.
    #[must_use]
    pub fn get_pane_sizing(&self) -> PaneSizing {
        self.pane_sizing
    }

    /// Set the pane sizing to `pane_sizing` and solve every tab again with
    /// it at its solve size ([`Session::resolve_tab_solve_size`]), in tab id
    /// order, each followed by step 1 of [`Session::update_tab_layout`]. After
    /// the last tab, step 2 runs once. Every tab is solved, whether or not
    /// `pane_sizing` or its solve size changed.
    ///
    /// Returns the events of those checks.
    #[must_use]
    pub fn update_pane_sizing(&mut self, pane_sizing: PaneSizing) -> Vec<Event> {
        self.pane_sizing = pane_sizing;
        let tab_ids: Vec<TabId> = self.tabs.keys().copied().collect();
        let mut emitted_events = Vec::new();
        for tab_id in tab_ids {
            let Some(tab_size) = self.resolve_tab_solve_size(tab_id) else {
                continue;
            };
            let Some(tab) = self.tabs.get_mut(&tab_id) else {
                continue;
            };
            tab.solved_tab_size = tab_size;
            let previous_layout_solve = tab.update_layout_solve(pane_sizing);
            emitted_events.extend(hand_off_suppressed_focus(
                self,
                tab_id,
                Some(&previous_layout_solve),
            ));
        }
        emitted_events.extend(self.minimize_undrawable_floating_panes());
        emitted_events
    }

    /// The tab size `tab_id` is solved at: [`Session::get_tab_size`] while an
    /// attached client views the tab, `0x0` while every client viewing it
    /// reports [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving),
    /// and the tab size the tab holds ([`Tab::get_solved_tab_size`]) while no
    /// client views it. `None` when the session holds no `tab_id`.
    ///
    /// A tab viewed by an `80x22` and a `100x30` pane area → `80x22`; a tab
    /// solved at `120x40` that no client views → `120x40`.
    #[must_use]
    pub fn resolve_tab_solve_size(&self, tab_id: TabId) -> Option<Size> {
        let tab = self.tabs.get(&tab_id)?;
        let is_tab_viewed = self
            .clients
            .list_attached_clients()
            .any(|client| client.get_active_tab_id() == tab_id);
        if !is_tab_viewed {
            return Some(tab.solved_tab_size);
        }
        Some(self.get_tab_size(tab_id).unwrap_or(Size {
            column_count: 0,
            row_count: 0,
        }))
    }

    /// Solve `tab_id` again at [`Session::resolve_tab_solve_size`] when that
    /// size differs from the one the tab is solved at. Then, whether or not
    /// the tab was solved again, run the focus and floating checks of
    /// [`Session::update_tab_layout`]: a client that switched to the tab with
    /// its focus on a suppressed pane moves.
    ///
    /// Returns the events of those checks. A tab id the session does not hold
    /// changes nothing and returns none.
    #[must_use]
    pub fn update_tab_solve(&mut self, tab_id: TabId) -> Vec<Event> {
        let Some(tab_size) = self.resolve_tab_solve_size(tab_id) else {
            return Vec::new();
        };
        let pane_sizing = self.pane_sizing;
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return Vec::new();
        };
        let previous_layout_solve = if tab.solved_tab_size == tab_size {
            None
        } else {
            tab.solved_tab_size = tab_size;
            Some(tab.update_layout_solve(pane_sizing))
        };
        self.apply_tab_solve_change(tab_id, previous_layout_solve.as_ref())
    }

    /// Replace `tab_id`'s layout tree with `layout_tree` and solve it again at
    /// the tab size the tab is solved at, with [`Session::get_pane_sizing`].
    /// Then:
    /// 1. every attached client that views the tab tiled, and whose focused
    ///    pane in it the new solve suppresses, focuses the pane that
    ///    inherits the focus, ranked against that pane's rect in the previous
    ///    solve, as a closed pane's focus is handed on;
    /// 2. every attached client that cannot draw floating panes
    ///    ([`Session::can_client_draw_floating_panes`]) minimizes the
    ///    floating panes it shows.
    ///
    /// Returns the events of steps 1 and 2. A tab id the session does not hold
    /// changes nothing and returns none.
    #[must_use]
    pub fn update_tab_layout(&mut self, tab_id: TabId, layout_tree: LayoutNode) -> Vec<Event> {
        let pane_sizing = self.pane_sizing;
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return Vec::new();
        };
        tab.layout = layout_tree;
        let previous_layout_solve = tab.update_layout_solve(pane_sizing);
        self.apply_tab_solve_change(tab_id, Some(&previous_layout_solve))
    }

    /// Steps 1 and 2 of [`Session::update_tab_layout`] for `tab_id`, with
    /// `previous_layout_solve` as the solve the tab held before its last
    /// solve. `None` ranks each suppressed pane against its rect in the
    /// tab's current solve.
    fn apply_tab_solve_change(
        &mut self,
        tab_id: TabId,
        previous_layout_solve: Option<&LayoutSolve>,
    ) -> Vec<Event> {
        let mut emitted_events = hand_off_suppressed_focus(self, tab_id, previous_layout_solve);
        emitted_events.extend(self.minimize_undrawable_floating_panes());
        emitted_events
    }

    /// The solve a client in `layout_mode` draws `tab_id` with. `Tiled` is the
    /// tab's own solve ([`Tab::get_layout_solve`]). `Fullscreen` on a leaf of
    /// the tab's tree gives that pane the whole tab and every other pane zero
    /// area at the tab's solved size, and suppresses the pane when that size
    /// is below its border-inclusive floor ([`solve_layout_with_mode`]).
    /// `Fullscreen` on a pane the tree does not hold is the tab's own solve.
    /// `None` when the session holds no `tab_id`.
    ///
    /// A `Tiled` client of an `80x22` tab gets the tab's solve with no copy.
    #[must_use]
    pub fn compute_client_solve(
        &self,
        tab_id: TabId,
        layout_mode: LayoutMode,
    ) -> Option<Cow<'_, LayoutSolve>> {
        let tab = self.tabs.get(&tab_id)?;
        let client_solve = match layout_mode {
            LayoutMode::Fullscreen { focused_pane_id } if tab.layout.has_pane(focused_pane_id) => {
                Cow::Owned(solve_layout_with_mode(
                    &tab.layout,
                    layout_mode,
                    Rect::from_size_at_origin(tab.solved_tab_size),
                    self.pane_sizing,
                ))
            }
            LayoutMode::Tiled | LayoutMode::Fullscreen { .. } => Cow::Borrowed(&tab.layout_solve),
        };
        Some(client_solve)
    }

    /// Whether `client` can draw floating panes: it has a floating viewport
    /// ([`Client::get_floating_viewport`], a pane area of at least `5x6`), and
    /// its own view of its active tab ([`Session::compute_client_solve`])
    /// has a pane that is not suppressed. A client whose active tab the
    /// session does not hold can draw floating panes while it has a floating
    /// viewport.
    ///
    /// A `4x6` pane area → `false`. An `80x22` pane area on a tab that a
    /// client with a `4x2` pane area also views → `false`: the tab is solved
    /// at `4x2`, and every pane is suppressed.
    #[must_use]
    pub fn can_client_draw_floating_panes(&self, client: &Client) -> bool {
        if client.get_floating_viewport().is_none() {
            return false;
        }
        let active_tab_id = client.get_active_tab_id();
        self.compute_client_solve(active_tab_id, client.get_layout_mode(active_tab_id))
            .is_none_or(|client_solve| !client_solve.is_every_pane_suppressed)
    }

    /// Minimize every floating pane shown by each attached client that cannot
    /// draw floating panes ([`Session::can_client_draw_floating_panes`]), in
    /// client id order. Each such client minimizes all the floating panes it
    /// shows ([`Client::minimize_shown_floating_panes`]), its input goes to its
    /// active tab's focused pane, and its placement revision advances. No
    /// other client's view changes.
    ///
    /// Returns, per changed client, one [`Event::PaneMinimizedChanged`] with
    /// `is_minimized: true` per pane, oldest shown first, then the
    /// [`Event::PaneFocused`] of its input ([`build_pane_focused_event`]).
    fn minimize_undrawable_floating_panes(&mut self) -> Vec<Event> {
        let minimizing_client_ids: Vec<ClientId> = self
            .clients
            .list_attached_clients()
            .filter(|client| {
                !client.list_shown_floating_pane_ids().is_empty()
                    && !self.can_client_draw_floating_panes(client)
            })
            .map(Client::get_client_id)
            .collect();
        let mut emitted_events = Vec::new();
        for client_id in minimizing_client_ids {
            let Some(client) = self.clients.get_client_mut_by_id(client_id) else {
                continue;
            };
            let previous_focused_pane_id = client.get_active_focused_pane_id();
            let minimized_pane_ids = client.minimize_shown_floating_panes();
            let _ = client.advance_placement_revision();
            emitted_events.extend(build_pane_minimized_changed_events(
                client_id,
                &minimized_pane_ids,
                true,
            ));
            emitted_events.extend(build_pane_focused_event(client, previous_focused_pane_id));
        }
        emitted_events
    }

    /// The cell size of the earliest-attached client viewing `tab_id` that
    /// reported one, ties broken by the lower client id. `None` when no client
    /// viewing `tab_id` reported a cell size.
    #[must_use]
    pub fn get_tab_cell_size(&self, tab_id: TabId) -> Option<PixelCellSize> {
        find_earliest_reported_cell_size(
            self.clients
                .list_attached_clients()
                .filter(|client| client.get_active_tab_id() == tab_id),
        )
    }

    /// The pane region to size the session's floating panes against: every
    /// attached client's floating viewport, whichever tab that client views,
    /// reduced to the per-axis minimum.
    ///
    /// Each attached client contributes its [`Client::get_floating_viewport`];
    /// a client that reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving), or a
    /// pane area smaller than `5x6` on either axis, contributes nothing.
    /// Returns `None` when no attached client contributes a size. A `120x40`
    /// client viewing tab `db` and an `80x22` client viewing tab `web` →
    /// `80x22`; a third client with a `4x40` pane area keeps `80x22`.
    #[must_use]
    pub fn get_shared_floating_viewport(&self) -> Option<Size> {
        self.clients
            .list_attached_clients()
            .filter_map(Client::get_floating_viewport)
            .reduce(Size::compute_minimum_axes)
    }

    /// The outer size a new floating pane that asks for `desired_size` takes
    /// in this session: [`solve_floating_pane_size`] against
    /// [`Session::get_shared_floating_viewport`], or the floating minimum
    /// ([`compute_floating_pane_minimum_size`]) of `pane_minimum_size` while no
    /// attached client has a floating viewport. `Percent(60)` by
    /// `Percent(60)` with an `80x22` shared viewport → `48x13`; with no client
    /// attached and a `2x1` pane minimum → `4x5`.
    #[must_use]
    pub fn compute_new_floating_pane_size(
        &self,
        desired_size: FloatingPaneSize,
        pane_minimum_size: Size,
    ) -> Size {
        match self.get_shared_floating_viewport() {
            Some(shared_floating_viewport) => {
                solve_floating_pane_size(desired_size, shared_floating_viewport, pane_minimum_size)
            }
            None => compute_floating_pane_minimum_size(pane_minimum_size),
        }
    }

    /// The rectangle the session places the floating pane `pane_id` in for
    /// `client`, counted in its pane-area cells: [`place_floating_pane`] of
    /// the client's position for the pane, the pane's solved size and cascade
    /// index, on the client's floating viewport
    /// ([`Client::get_floating_viewport`]). The client's minimized state is
    /// not read. `None` when `pane_id` is no floating member of this session,
    /// the client holds no view of it, or the client has no floating
    /// viewport.
    ///
    /// A `40x12` pane pinned at `(20, 5)`, on a client whose floating viewport
    /// is `50x22` → `(10, 5)` `40x12`.
    #[must_use]
    pub fn find_client_floating_pane_rect(&self, client: &Client, pane_id: PaneId) -> Option<Rect> {
        let floating_viewport = client.get_floating_viewport()?;
        let (cascade_index, floating_member) = self.floating_set.find_member(pane_id)?;
        Some(place_floating_pane(
            client.get_floating_pane_view(pane_id)?.position,
            floating_member.solved_size,
            cascade_index,
            floating_viewport,
        ))
    }

    /// The cell size the session's floating panes take: that of the
    /// earliest-attached client that reported one, whichever tab that client
    /// views, ties broken by the lower client id. `None` when no attached client
    /// reported a cell size.
    #[must_use]
    pub fn get_floating_cell_size(&self) -> Option<PixelCellSize> {
        find_earliest_reported_cell_size(self.clients.list_attached_clients())
    }

    /// Add `floating_member` to the floating set as its newest member, and a
    /// minimized view of it to every attached client
    /// ([`Client::add_floating_pane_view`]): at `acting_view`'s position for
    /// `acting_view`'s client, and at the default placement for every other
    /// client.
    ///
    /// # Errors
    ///
    /// [`FloatingSetError::DuplicatePane`] when the set already holds
    /// `floating_member.pane_id`, else [`FloatingSetError::TooManyPanes`] when
    /// [`FloatingSet::is_full`]. The session does not change.
    pub fn add_floating_member(
        &mut self,
        floating_member: FloatingMember,
        acting_view: Option<(ClientId, FloatingPanePosition)>,
    ) -> Result<(), FloatingSetError> {
        self.floating_set.add_member(floating_member)?;
        for client in self.clients.list_attached_clients_mut() {
            let position = match acting_view {
                Some((acting_client_id, acting_position))
                    if acting_client_id == client.get_client_id() =>
                {
                    acting_position
                }
                _ => FloatingPanePosition::Default,
            };
            client.add_floating_pane_view(floating_member.pane_id, position);
        }
        Ok(())
    }

    /// Remove `pane_id` from the floating set, keeping the order of the other
    /// members, and from every attached client's floating view: its view and
    /// its shown floating pane entry. A client whose floating focus was on
    /// `pane_id` focuses the newest floating pane it still shows, else no
    /// floating pane.
    /// Returns the removed member, or `None`, changing nothing, when `pane_id`
    /// is not floating.
    pub fn remove_floating_member(&mut self, pane_id: PaneId) -> Option<FloatingMember> {
        let (member_index, _) = self.floating_set.find_member(pane_id)?;
        let floating_member = self.floating_set.members.remove(member_index);
        for client in self.clients.list_attached_clients_mut() {
            client.remove_floating_pane_view(pane_id);
        }
        Some(floating_member)
    }

    /// Request shutdown: move a `Starting`, `Running` or `Detaching` session to
    /// `Stopping`. State is retained: stopping destroys no tabs, panes or
    /// clients.
    pub fn request_session_stop(&mut self) {
        // Idempotent: requesting a stop on an already-`Stopping`/`Stopped`
        // session is rejected and changes nothing.
        let _ = self.update_lifecycle(SessionLifecycleEvent::StopRequested);
    }

    /// Finish shutdown once teardown is done, moving `Stopping` to the terminal
    /// `Stopped`.
    pub fn complete_session_stop(&mut self) {
        // Only a `Stopping` session completes; any other state rejects it.
        let _ = self.update_lifecycle(SessionLifecycleEvent::StopCompleted);
    }

    /// Check every cross-store invariant and return *all* violations in one
    /// pass, or `Ok(())` when the session is internally consistent.
    ///
    /// The checks run in this order:
    ///
    /// 1. Each tab's map key, then each of its layout leaves against the pane
    ///    registry, then its stored solve against a fresh solve of its tree,
    ///    then the tab size it is solved at against
    ///    [`Session::resolve_tab_solve_size`], tab by tab.
    /// 2. Each bar index that two tabs claim.
    /// 3. Each pane that two layouts hold.
    /// 4. The number of floating members against
    ///    [`MAX_FLOATING_PANES_PER_SESSION`], then each floating member against
    ///    the pane registry, the other members and the layouts.
    /// 5. Each registry record against the layouts and the floating members.
    /// 6. Each attached client's session id, active tab, focus, zoom,
    ///    floating views, shown floating panes, floating focus, floating panes
    ///    shown while it cannot draw them, and a focus on a pane its tiled view
    ///    suppresses.
    ///
    /// See [`SessionConsistencyError`] for the individual checks. Each check
    /// walks its subjects by id, by bar index, in the order the panes became
    /// floating, or in shown order: one session reports the same list on every
    /// call.
    pub fn validate_session_consistency(&self) -> Result<(), Vec<SessionConsistencyError>> {
        let mut consistency_violations = vec![];
        // Pane id -> the tabs whose layout holds it as a leaf, in pane id
        // order. Built once here, then read to check the leaf/registry
        // relationship in both directions.
        let mut tab_ids_by_pane_id: BTreeMap<PaneId, Vec<TabId>> = BTreeMap::new();
        // Bar position -> how many tabs claim it.
        let mut tab_count_by_index: BTreeMap<usize, usize> = BTreeMap::new();

        for (tab_id, tab) in self.tabs.iter() {
            // Every tab is keyed under its own id.
            if *tab_id != tab.tab_id {
                consistency_violations.push(SessionConsistencyError::TabKeyMismatch {
                    stored_tab_id: *tab_id,
                    reported_tab_id: tab.tab_id,
                });
            }

            *tab_count_by_index.entry(tab.tab_index).or_insert(0) += 1;

            for pane_id in tab.layout.list_leaf_pane_ids() {
                tab_ids_by_pane_id
                    .entry(pane_id)
                    .or_default()
                    .push(tab.tab_id);

                if self.panes.get_pane_record_by_id(pane_id).is_none() {
                    consistency_violations.push(SessionConsistencyError::PaneNotInRegistry {
                        tab_id: tab.tab_id,
                        pane_id,
                    });
                }
            }

            let fresh_layout_solve = solve_layout_with_sizing(
                &tab.layout,
                Rect::from_size_at_origin(tab.solved_tab_size),
                self.pane_sizing,
            );
            if fresh_layout_solve != tab.layout_solve {
                consistency_violations
                    .push(SessionConsistencyError::TabSolveStale { tab_id: *tab_id });
            }
            if let Some(tab_size) = self.resolve_tab_solve_size(*tab_id) {
                if tab_size != tab.solved_tab_size {
                    consistency_violations.push(SessionConsistencyError::TabSolveSizeStale {
                        tab_id: *tab_id,
                        solved_tab_size: tab.solved_tab_size,
                        tab_size,
                    });
                }
            }
        }

        // No two tabs may claim the same bar position.
        for (tab_index, tab_count) in &tab_count_by_index {
            if *tab_count > 1 {
                consistency_violations.push(SessionConsistencyError::DuplicateTabIndex {
                    tab_index: *tab_index,
                });
            }
        }

        // A pane is a layout leaf at most once across all tabs.
        for (pane_id, tab_ids) in &tab_ids_by_pane_id {
            if tab_ids.len() > 1 {
                consistency_violations.push(SessionConsistencyError::PaneInMultipleLayouts {
                    pane_id: *pane_id,
                    tab_ids: tab_ids.clone(),
                });
            }
        }

        // The floating set holds at most `MAX_FLOATING_PANES_PER_SESSION`
        // entries.
        let floating_member_count = self.floating_set.list_members().len();
        if floating_member_count > MAX_FLOATING_PANES_PER_SESSION {
            consistency_violations.push(SessionConsistencyError::TooManyFloatingPanes {
                member_count: floating_member_count,
            });
        }

        // Pane id -> how many times the floating set lists it.
        let mut floating_member_count_by_pane_id: BTreeMap<PaneId, usize> = BTreeMap::new();
        for floating_member in self.floating_set.list_members() {
            *floating_member_count_by_pane_id
                .entry(floating_member.pane_id)
                .or_insert(0) += 1;
        }

        // A floating member is a registry pane, listed once, and a leaf of no
        // layout.
        for (&pane_id, &member_count) in &floating_member_count_by_pane_id {
            if self.panes.get_pane_record_by_id(pane_id).is_none() {
                consistency_violations
                    .push(SessionConsistencyError::FloatingPaneNotInRegistry { pane_id });
            }
            if member_count > 1 {
                consistency_violations
                    .push(SessionConsistencyError::DuplicateFloatingPane { pane_id });
            }
            if let Some(tab_ids) = tab_ids_by_pane_id.get(&pane_id) {
                consistency_violations.push(SessionConsistencyError::FloatingPaneInLayout {
                    pane_id,
                    tab_ids: tab_ids.clone(),
                });
            }
        }

        // Every record is a leaf somewhere or a floating member.
        for pane_record in self.panes.list_pane_records() {
            let pane_id = pane_record.get_pane_id();
            if !tab_ids_by_pane_id.contains_key(&pane_id)
                && !floating_member_count_by_pane_id.contains_key(&pane_id)
            {
                consistency_violations
                    .push(SessionConsistencyError::OrphanedPaneRecord { pane_id });
            }
        }

        for client in self.clients.list_attached_clients() {
            // A client in this registry must belong to this session.
            if client.get_session_id() != self.session_id {
                consistency_violations.push(SessionConsistencyError::ClientSessionMismatch {
                    client_id: client.get_client_id(),
                    found_session_id: client.get_session_id(),
                });
            }

            // The tab a client is currently showing must exist. Checked only
            // while the session still holds tabs: a session emptied by its last
            // tab closing leaves every client's `active_tab_id` naming that closed
            // tab until the transport disconnects them.
            if !self.tabs.is_empty() && !self.tabs.contains_key(&client.get_active_tab_id()) {
                consistency_violations.push(SessionConsistencyError::ActiveTabMissing {
                    client_id: client.get_client_id(),
                    tab_id: client.get_active_tab_id(),
                });
            }

            // Each remembered focus must point at a real pane that is a leaf of
            // the tab it was focused in.
            for (&tab_id, &focused_pane_id) in client
                .list_focused_pane_ids()
                .iter()
                .collect::<BTreeMap<_, _>>()
            {
                if self.panes.get_pane_record_by_id(focused_pane_id).is_none() {
                    consistency_violations.push(SessionConsistencyError::FocusPaneNotInRegistry {
                        client_id: client.get_client_id(),
                        tab_id,
                        pane_id: focused_pane_id,
                    });
                }

                match self.tabs.get(&tab_id) {
                    None => consistency_violations.push(SessionConsistencyError::FocusTabMissing {
                        client_id: client.get_client_id(),
                        tab_id,
                    }),
                    Some(tab) if !tab.layout.has_pane(focused_pane_id) => {
                        consistency_violations.push(SessionConsistencyError::FocusTargetMissing {
                            client_id: client.get_client_id(),
                            tab_id,
                            pane_id: focused_pane_id,
                        });
                    }
                    Some(_) => {}
                }
            }

            // The pane a client is zoomed on must have a registry record and be
            // a leaf of the tab it is zoomed in. Removing a pane drops every
            // zoom on it.
            for (&tab_id, &zoomed_pane_id) in client
                .list_zoomed_pane_ids()
                .iter()
                .collect::<BTreeMap<_, _>>()
            {
                let is_live_leaf = self.panes.get_pane_record_by_id(zoomed_pane_id).is_some()
                    && self
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| tab.layout.has_pane(zoomed_pane_id));
                if !is_live_leaf {
                    consistency_violations.push(SessionConsistencyError::ZoomTargetMissing {
                        client_id: client.get_client_id(),
                        tab_id,
                        pane_id: zoomed_pane_id,
                    });
                }
            }

            // The client holds a floating view of each floating member and of
            // nothing else, and a view says minimized exactly when the shown
            // floating panes do not list its pane.
            for &viewed_pane_id in client
                .list_floating_pane_views()
                .keys()
                .collect::<BTreeSet<_>>()
            {
                if !floating_member_count_by_pane_id.contains_key(&viewed_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::FloatingViewTargetMissing {
                            client_id: client.get_client_id(),
                            pane_id: viewed_pane_id,
                        },
                    );
                }
            }
            for floating_member in self.floating_set.list_members() {
                let pane_id = floating_member.pane_id;
                let Some(floating_pane_view) = client.get_floating_pane_view(pane_id) else {
                    consistency_violations.push(SessionConsistencyError::FloatingViewMissing {
                        client_id: client.get_client_id(),
                        pane_id,
                    });
                    continue;
                };
                let is_listed_shown = client.list_shown_floating_pane_ids().contains(&pane_id);
                if floating_pane_view.is_minimized == is_listed_shown {
                    consistency_violations.push(
                        SessionConsistencyError::FloatingPaneShownStateMismatch {
                            client_id: client.get_client_id(),
                            pane_id,
                            is_minimized: floating_pane_view.is_minimized,
                        },
                    );
                }
            }

            // Each shown floating pane is a floating member, listed once.
            let mut shown_pane_ids: BTreeSet<PaneId> = BTreeSet::new();
            for &shown_pane_id in client.list_shown_floating_pane_ids() {
                if !floating_member_count_by_pane_id.contains_key(&shown_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::ShownFloatingPaneMissing {
                            client_id: client.get_client_id(),
                            pane_id: shown_pane_id,
                        },
                    );
                }
                if !shown_pane_ids.insert(shown_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::DuplicateShownFloatingPane {
                            client_id: client.get_client_id(),
                            pane_id: shown_pane_id,
                        },
                    );
                }
            }

            // The floating focus is the newest shown floating pane, and no
            // focus means none is shown.
            let newest_shown_pane_id = client.list_shown_floating_pane_ids().last().copied();
            if client.get_focused_floating_pane_id() != newest_shown_pane_id {
                consistency_violations.push(SessionConsistencyError::FloatingFocusNotNewestShown {
                    client_id: client.get_client_id(),
                    focused_pane_id: client.get_focused_floating_pane_id(),
                    newest_shown_pane_id,
                });
            }

            // A client that cannot draw floating panes shows none.
            if !self.can_client_draw_floating_panes(client) {
                for &shown_pane_id in client.list_shown_floating_pane_ids() {
                    consistency_violations.push(
                        SessionConsistencyError::UndrawableFloatingPaneShown {
                            client_id: client.get_client_id(),
                            pane_id: shown_pane_id,
                        },
                    );
                }
            }

            // A client that views its active tab tiled focuses no pane that
            // the tab's solve suppresses, while any pane of the tab is drawn.
            let active_tab_id = client.get_active_tab_id();
            if let (LayoutMode::Tiled, Some(tab), Some(focused_pane_id)) = (
                client.get_layout_mode(active_tab_id),
                self.tabs.get(&active_tab_id),
                client.get_focused_pane_id(active_tab_id),
            ) {
                if !tab.layout_solve.is_every_pane_suppressed
                    && tab
                        .layout_solve
                        .suppressed_pane_ids
                        .contains(&focused_pane_id)
                {
                    consistency_violations.push(SessionConsistencyError::SuppressedPaneFocused {
                        client_id: client.get_client_id(),
                        tab_id: active_tab_id,
                        pane_id: focused_pane_id,
                    });
                }
            }
        }

        if consistency_violations.is_empty() {
            Ok(())
        } else {
            Err(consistency_violations)
        }
    }
}

/// The cell size of the earliest-attached of `clients` that reported one,
/// ties broken by the lower client id. `None` when none of `clients` reported
/// a cell size.
fn find_earliest_reported_cell_size<'a>(
    clients: impl Iterator<Item = &'a Client>,
) -> Option<PixelCellSize> {
    clients
        .filter(|client| client.get_cell_size().is_some())
        .min_by_key(|client| (client.get_attached_at(), client.get_client_id()))
        .and_then(Client::get_cell_size)
}

#[cfg(test)]
pub(crate) mod tests;
