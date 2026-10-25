//! Tests for the viewer half: construction, viewport updates, the settings and
//! colors it reads from its own config files, the keymap it validates before
//! trusting, the hints one frame is painted from, and what it takes off its
//! subscription — live events, the frame it resumes from after a lag, and the
//! items meant for a client in another process — the lock mode it shows
//! until a frame names the lock command it sent, and the pane placement mode
//! it keeps from the placement preview and the last applied placement each
//! frame carries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_config::hints::{KeyMatch, KeymapHints};
use koshi_config::key::Leader;
use koshi_config::layer::{PartialColorPalette, PartialLayoutDefaults};
use koshi_config::types::{
    BoundAction, KeybindingsConfig, ModeBindings, ModeName, RgbColor, WheelScroll,
};
use koshi_core::action::ActionReference;
use koshi_core::command::{PanePlacementAnchor, TiledPlacementTarget};
use koshi_core::geometry::{Direction, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{PlacementTabSizes, SwapTabSizes};
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    list_pane_slots, ClientSnapshot, FloatingPane, PaneSlot, PlacementPreview, PlacementSnapshot,
    PlacementSource, PlacementTabSnapshot, Reconnecting, RenderSnapshot, SessionSnapshot,
    TabMetadata, TabSnapshot,
};

use super::*;

pub(crate) use koshi_test_support::fixtures::build_key_input_for_chord;

/// How long a test thread sleeps between two checks of a condition another
/// thread makes true.
pub(crate) const TEST_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(1);

/// The terminal size every fixture in this crate's tests is built at.
pub(crate) const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// Seed a session named `S-example` on `server`, holding one tab with one
/// shell pane, as the session server does, then attach a fresh local client to
/// that tab with a [`TEST_VIEWPORT_SIZE`] terminal. Returns the client's id.
pub(crate) fn create_session_with_attached_client(
    server: &mut koshi_runtime::server::Server,
) -> ClientId {
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "S-example".to_string(),
            TEST_VIEWPORT_SIZE,
            std::time::SystemTime::UNIX_EPOCH,
        )
        .expect("seed the session");
    let tab_id = *server.list_sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .expect("the seeded tab");
    let client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        client_id,
        TEST_VIEWPORT_SIZE,
        None,
        tab_id,
        None,
        std::time::SystemTime::UNIX_EPOCH,
        false,
    );
    client_id
}

/// A destination tab size that no placement into a tab of two panes fits:
/// `4x2`.
pub(crate) const NO_ROOM_TAB_SIZE: Size = Size {
    column_count: 4,
    row_count: 2,
};

/// The binding of the core action `action_name`.
pub(crate) fn build_core_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("valid core action name"),
    }
}

/// A client on the built-in defaults.
///
/// Every test module in this crate builds its clients from here.
pub(crate) fn build_test_client() -> Client {
    Client::from_client_id_and_viewport_size(
        ClientId::new(),
        TEST_VIEWPORT_SIZE,
        TerminalCleanupGuard::new(),
    )
}

/// Tab `tab_id`, named `tab_name`, whose one pane `pane_id` fills `80x24`
/// cells, as a placement preview holds it.
fn build_one_pane_placement_tab_snapshot(
    tab_id: TabId,
    tab_name: &str,
    pane_id: PaneId,
) -> PlacementTabSnapshot {
    let tab_rect = Rect::from_size_at_origin(TEST_VIEWPORT_SIZE);
    PlacementTabSnapshot {
        layout_tree: LayoutNode::Pane(pane_id),
        focus_history: Vec::new(),
        tab_snapshot: TabSnapshot {
            tab_id,
            tab_name: tab_name.to_owned(),
            pane_slots: vec![PaneSlot {
                pane_id,
                outer_rect: tab_rect,
                content_rect: Some(tab_rect),
                is_visible: true,
                is_suppressed: false,
            }],
            tab_size: TEST_VIEWPORT_SIZE,
            stack_headers: Vec::new(),
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: false,
            gap_cell_count: 0,
        },
    }
}

/// A placement preview of `source_pane_id` from tab `source_tab_id`, a
/// one-pane tab named `source`, into tab `destination_tab_id`, a one-pane tab
/// named `destination` of a new pane, as the renderer reads it. Each tab is
/// `80x24` cells, a pane is at least `1x1` cells, and panes have no gap. The
/// session checks an insertion, and a swap with the new pane, at `80x24`.
pub(crate) fn build_test_placement_snapshot(
    source_pane_id: PaneId,
    source_tab_id: TabId,
    destination_tab_id: TabId,
) -> PlacementSnapshot {
    let destination_pane_id = PaneId::new();
    PlacementSnapshot {
        source_pane_id,
        source: PlacementSource::Tiled(build_one_pane_placement_tab_snapshot(
            source_tab_id,
            "source",
            source_pane_id,
        )),
        destination_tab_id,
        destination_tab_snapshot: Some(build_one_pane_placement_tab_snapshot(
            destination_tab_id,
            "destination",
            destination_pane_id,
        )),
        pane_sizing: PaneSizing {
            minimum_size: Size {
                column_count: 1,
                row_count: 1,
            },
            gap_cell_count: 0,
        },
        insertion_tab_sizes: PlacementTabSizes {
            destination_tab_size: Some(TEST_VIEWPORT_SIZE),
            source_tab_size: None,
        },
        swap_tab_sizes: vec![SwapTabSizes {
            target_pane_id: destination_pane_id,
            tab_sizes: PlacementTabSizes {
                destination_tab_size: Some(TEST_VIEWPORT_SIZE),
                source_tab_size: Some(TEST_VIEWPORT_SIZE),
            },
        }],
    }
}

/// A same-tab placement preview of `layout_tree` for moving `source_pane_id`,
/// with every slot and stack header where the tiled solve over the test
/// viewport puts it ([`list_pane_slots`]). A collapsed stack member's slot is
/// its header strip and is not visible.
pub(crate) fn build_solved_placement_snapshot(
    source_pane_id: PaneId,
    tab_id: TabId,
    layout_tree: LayoutNode,
) -> PlacementSnapshot {
    let mut placement_snapshot = build_test_placement_snapshot(source_pane_id, tab_id, tab_id);
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.swap_tab_sizes.clear();
    let layout_solve = solve_layout_with_mode(
        &layout_tree,
        LayoutMode::Tiled,
        Rect::from_size_at_origin(TEST_VIEWPORT_SIZE),
        placement_snapshot.pane_sizing,
    );
    let source_tab_snapshot = get_source_tab_snapshot_mut(&mut placement_snapshot);
    source_tab_snapshot.tab_snapshot.pane_slots = list_pane_slots(&layout_solve);
    source_tab_snapshot.tab_snapshot.stack_headers = layout_solve.stack_headers;
    source_tab_snapshot.layout_tree = layout_tree;
    placement_snapshot
}

/// The one pane of the destination tab of `placement_snapshot`.
///
/// # Panics
/// When the preview holds no destination tab of one pane.
pub(crate) fn get_destination_pane_id(placement_snapshot: &PlacementSnapshot) -> PaneId {
    let destination_tab_snapshot = placement_snapshot
        .destination_tab_snapshot
        .as_ref()
        .expect("the preview holds a destination tab");
    let LayoutNode::Pane(destination_pane_id) = destination_tab_snapshot.layout_tree else {
        panic!("the destination tab holds one pane");
    };
    destination_pane_id
}

/// The source tab of `placement_snapshot`, for in-place edits.
///
/// # Panics
/// When the source pane floats.
pub(crate) fn get_source_tab_snapshot_mut(
    placement_snapshot: &mut PlacementSnapshot,
) -> &mut PlacementTabSnapshot {
    let PlacementSource::Tiled(source_tab_snapshot) = &mut placement_snapshot.source else {
        panic!("the test placement preview's source pane is tiled");
    };
    source_tab_snapshot
}

/// Float the source pane of `placement_snapshot`: its source tab becomes the
/// destination tab, and the preview checks no swap.
///
/// # Panics
/// When the source pane already floats.
pub(crate) fn float_placement_source_pane(placement_snapshot: &mut PlacementSnapshot) {
    let PlacementSource::Tiled(source_tab_snapshot) =
        std::mem::replace(&mut placement_snapshot.source, PlacementSource::Floating)
    else {
        panic!("the test placement preview's source pane is tiled");
    };
    placement_snapshot.destination_tab_snapshot = Some(source_tab_snapshot);
    placement_snapshot.swap_tab_sizes.clear();
}

/// Hold `placement_snapshot` as the available preview `client` received.
pub(crate) fn hold_placement_snapshot(client: &mut Client, placement_snapshot: PlacementSnapshot) {
    client.placement_state.placement_preview = Some(Arc::new(PlacementPreview::Available(
        Box::new(placement_snapshot),
    )));
}

/// A frame for `client`, in its lock mode and mouse-select mode, on
/// `active_tab_id` of the tabs `tab_ids`, where `focused_pane_id` takes the
/// input. It names `last_applied_placement_command_id` as the client's last
/// applied placement and carries `placement_preview`. The active tab holds no
/// pane slot.
pub(crate) fn build_placement_frame(
    client: &Client,
    active_tab_id: TabId,
    tab_ids: &[TabId],
    focused_pane_id: Option<PaneId>,
    last_applied_placement_command_id: Option<CommandId>,
    placement_preview: Option<PlacementPreview>,
) -> RenderSnapshot {
    let mut render_snapshot = *build_render_snapshot(
        client.get_client_id(),
        client.lock_mode,
        client.is_mouse_selection_enabled,
    );
    render_snapshot.session_snapshot.active_tab_snapshot.tab_id = active_tab_id;
    render_snapshot.session_snapshot.tabs_metadata = tab_ids
        .iter()
        .enumerate()
        .map(|(tab_index, &tab_id)| TabMetadata {
            tab_id,
            tab_name: String::from("tab"),
            tab_index,
            is_active: tab_id == active_tab_id,
        })
        .collect();
    render_snapshot.client_snapshot.active_tab_id = active_tab_id;
    render_snapshot.client_snapshot.focused_pane_id = focused_pane_id;
    render_snapshot.client_snapshot.active_tab_focused_pane_id = focused_pane_id;
    render_snapshot
        .client_snapshot
        .last_applied_placement_command_id = last_applied_placement_command_id;
    render_snapshot.placement_preview = placement_preview.map(Arc::new);
    render_snapshot
}

/// Apply to `client` a frame that keeps its active tab, its focused pane and
/// its tab list ([`build_placement_frame`]), names
/// `last_applied_placement_command_id`, and carries `placement_preview`.
///
/// # Panics
/// When `client` holds no frame view.
pub(crate) fn apply_frame_keeping_view(
    client: &mut Client,
    last_applied_placement_command_id: Option<CommandId>,
    placement_preview: Option<PlacementPreview>,
) {
    let active_tab_id = client.active_tab_id.expect("the client holds a frame view");
    let tab_ids = client.visible_tab_ids.clone();
    let render_snapshot = build_placement_frame(
        client,
        active_tab_id,
        &tab_ids,
        client.focused_pane_id,
        last_applied_placement_command_id,
        placement_preview,
    );
    client.apply_render_snapshot(&render_snapshot);
}

/// The insertion at the right edge of the whole tab `destination_tab_id`.
pub(crate) fn build_whole_tab_insertion_target(destination_tab_id: TabId) -> TiledPlacementTarget {
    TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    }
}

/// A client on tab `source_tab_id` of the tabs `[source_tab_id,
/// destination_tab_id]`, focusing `source_pane_id`, in pane placement mode
/// that places `source_pane_id` into `destination_tab_id` with no target, until
/// Esc.
fn build_client_placing_across_tabs(
    source_pane_id: PaneId,
    source_tab_id: TabId,
    destination_tab_id: TabId,
) -> Client {
    let mut client = build_test_client();
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client
}

#[test]
fn a_same_tab_insertion_that_preserves_the_layout_sends_no_command() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let mut placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, source_tab_id);
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.swap_tab_sizes.clear();
    get_source_tab_snapshot_mut(&mut placement_snapshot).layout_tree =
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(target_pane_id),
                LayoutNode::Pane(source_pane_id),
            ],
        ));
    let placement_target = TiledPlacementTarget::Split {
        destination_tab_id: source_tab_id,
        anchor: PanePlacementAnchor::Pane(target_pane_id),
        direction: Direction::Right,
    };
    hold_placement_snapshot(&mut client, placement_snapshot);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(placement_target),
        pending_placement_command_id: None,
    });

    assert_eq!(
        client.submit_placement_command(),
        Some(PlacementInputAction::Consumed)
    );
    assert_eq!(client.get_placement_target(), None);
    assert!(!client.is_placement_confirmation_pending());
}

#[test]
fn a_frame_holds_only_the_preview_of_the_source_pane_and_destination_tab_of_the_mode() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let other_tab_placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, TabId::new());
    let placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, destination_tab_id);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(
            other_tab_placement_snapshot,
        ))),
    );

    assert_eq!(client.find_held_placement_preview(), None);
    assert!(client.is_placement_preview_loading());
    assert_eq!(client.get_placement_target(), None);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(
            placement_snapshot.clone(),
        ))),
    );

    assert_eq!(client.get_placement_snapshot(), Some(&placement_snapshot));
    assert!(!client.is_placement_preview_loading());
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(build_whole_tab_insertion_target(destination_tab_id))
    );
}

#[test]
fn an_accepted_preview_sets_the_source_tab_to_the_tab_the_preview_names() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let current_source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    client.set_frame_view(
        destination_tab_id,
        Some(PaneId::new()),
        Some(PaneId::new()),
        vec![destination_tab_id, current_source_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(TabId::new()),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    let placement_snapshot =
        build_test_placement_snapshot(source_pane_id, current_source_tab_id, destination_tab_id);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(placement_snapshot))),
    );

    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.source_tab_id),
        Some(current_source_tab_id)
    );
}

#[test]
fn an_unavailable_preview_ends_pane_placement_mode() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Unavailable {
            source_pane_id,
            destination_tab_id,
        }),
    );

    assert_eq!(client.placement_state.placement_mode, None);
    assert!(!client.is_placement_mode_active());
}

#[test]
fn an_unavailable_preview_of_a_tab_the_mode_no_longer_previews_keeps_pane_placement_mode() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let current_destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, current_destination_tab_id);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Unavailable {
            source_pane_id,
            destination_tab_id: TabId::new(),
        }),
    );

    assert!(client.is_placement_mode_active());
    assert_eq!(
        client.get_placement_destination_tab_id(),
        Some(current_destination_tab_id)
    );
}

#[test]
fn an_unavailable_preview_waits_for_the_answer_to_a_confirmation_then_ends_the_mode() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let command_id = CommandId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_target = build_whole_tab_insertion_target(destination_tab_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(placement_target.clone());
    placement_mode.pending_placement_command_id = Some(command_id);

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Unavailable {
            source_pane_id,
            destination_tab_id,
        }),
    );

    assert!(client.is_pending_placement_command(command_id));
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(placement_target)
    );

    assert!(client.reject_placement_command(command_id));

    assert_eq!(client.placement_state.placement_mode, None);
    assert!(!client.is_placement_confirmation_pending());
}

#[test]
fn clearing_the_placement_draft_drops_an_unconfirmed_target() {
    let mut client = build_test_client();
    let destination_tab_id = TabId::new();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Pane(PaneId::new()),
            direction: Direction::Right,
        }),
        pending_placement_command_id: None,
    });

    client.clear_placement_draft();

    assert_eq!(client.get_placement_target(), None);
    assert!(client.is_placement_mode_active());
    assert!(!client.is_placement_confirmation_pending());
}

#[test]
fn a_frame_that_names_no_applied_placement_keeps_the_confirmation_waiting() {
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    let command_id = CommandId::new();
    record_pending_placement_command(&mut client, command_id);
    let submitted_placement_target = client.get_placement_target().cloned();

    apply_frame_keeping_view(&mut client, None, None);

    assert!(client.is_pending_placement_command(command_id));
    assert_eq!(
        client.get_placement_target().cloned(),
        submitted_placement_target
    );
}

#[test]
fn a_frame_naming_another_placement_keeps_the_confirmation_until_the_session_rejects_it() {
    let source_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let command_id = CommandId::new();
    let other_command_id = CommandId::new();
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        source_pane_id,
        destination_tab_id,
    );
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    record_pending_placement_command(&mut client, command_id);
    let submitted_placement_target = client
        .get_placement_target()
        .cloned()
        .expect("the submitted placement has a target");

    apply_frame_keeping_view(&mut client, Some(other_command_id), None);

    assert!(client.is_pending_placement_command(command_id));
    assert!(!client.is_pending_placement_command(other_command_id));
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(submitted_placement_target)
    );
    assert!(!client.placement_state.needs_placement_reconciliation);

    assert!(client.reject_placement_command(command_id));

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert!(!client.is_placement_confirmation_pending());
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn the_next_confirmation_waits_for_a_frame_that_names_its_own_id() {
    let target_pane_id = PaneId::new();
    let first_command_id = CommandId::new();
    let next_command_id = CommandId::new();
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    record_pending_placement_command(&mut client, first_command_id);
    apply_frame_keeping_view(&mut client, Some(first_command_id), None);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode stays on after the accepted placement");
    placement_mode.placement_target = Some(TiledPlacementTarget::Swap { target_pane_id });
    placement_mode.pending_placement_command_id = Some(next_command_id);

    apply_frame_keeping_view(&mut client, Some(first_command_id), None);

    assert!(client.is_pending_placement_command(next_command_id));
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(TiledPlacementTarget::Swap { target_pane_id })
    );
}

#[test]
fn a_frame_naming_the_confirmed_placement_keeps_keyboard_placement_mode_with_no_target() {
    let command_id = CommandId::new();
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    record_pending_placement_command(&mut client, command_id);

    apply_frame_keeping_view(&mut client, Some(command_id), None);

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(client.get_placement_target(), None);
    assert!(!client.is_placement_confirmation_pending());
    assert!(!client.placement_state.needs_placement_reconciliation);
}

#[test]
fn a_changed_preview_keeps_a_target_the_session_accepts_in_it() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, destination_tab_id);
    let placement_target = TiledPlacementTarget::Swap {
        target_pane_id: get_destination_pane_id(&placement_snapshot),
    };
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .placement_target = Some(placement_target.clone());

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(placement_snapshot))),
    );

    assert_eq!(
        client.get_placement_target().cloned(),
        Some(placement_target)
    );
}

#[test]
fn a_changed_preview_clears_a_target_the_session_refuses_and_selects_the_whole_tab() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .placement_target = Some(TiledPlacementTarget::Swap {
        target_pane_id: PaneId::new(),
    });

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(
            build_test_placement_snapshot(source_pane_id, source_tab_id, destination_tab_id),
        ))),
    );

    assert_eq!(
        client.get_placement_target().cloned(),
        Some(build_whole_tab_insertion_target(destination_tab_id))
    );
}

#[test]
fn a_placement_target_outcome_follows_the_held_preview_and_the_selected_target() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, destination_tab_id);
    let split_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(get_destination_pane_id(&placement_snapshot)),
        direction: Direction::Right,
    };
    let mut no_room_placement_snapshot = placement_snapshot.clone();
    no_room_placement_snapshot
        .insertion_tab_sizes
        .destination_tab_size = Some(NO_ROOM_TAB_SIZE);
    let set_placement_target =
        |client: &mut Client, placement_target: Option<TiledPlacementTarget>| {
            client
                .placement_state
                .placement_mode
                .as_mut()
                .expect("pane placement mode is on")
                .placement_target = placement_target;
        };
    hold_placement_snapshot(&mut client, placement_snapshot.clone());

    client.update_placement_target_outcome();
    assert_eq!(client.get_placement_target_outcome(), None, "no target");

    set_placement_target(&mut client, Some(split_target.clone()));
    client.update_placement_target_outcome();
    let held_placement_preview = Arc::clone(
        client
            .find_held_placement_preview()
            .expect("the preview is held"),
    );
    assert_eq!(
        client.get_placement_target_outcome(),
        Some(&PlacementTargetOutcome {
            placement_preview: held_placement_preview,
            placement_target: split_target.clone(),
            placed_preview: placement_snapshot.build_placed_preview(&split_target),
        })
    );
    assert_eq!(client.find_placement_refusal(), None);

    hold_placement_snapshot(&mut client, no_room_placement_snapshot.clone());
    client.update_placement_target_outcome();
    assert_eq!(
        client.find_placement_refusal(),
        Some(&PlacementPreviewError::NoRoom {
            tab_id: destination_tab_id
        }),
        "a new preview with the same target"
    );

    // A swap into another tab is checked at its swap tab sizes, which the
    // no-room preview keeps.
    let swap_target = TiledPlacementTarget::Swap {
        target_pane_id: get_destination_pane_id(&placement_snapshot),
    };
    set_placement_target(&mut client, Some(swap_target.clone()));
    client.update_placement_target_outcome();
    assert_eq!(
        client
            .get_placement_target_outcome()
            .map(|placement_target_outcome| {
                (
                    &placement_target_outcome.placement_target,
                    &placement_target_outcome.placed_preview,
                )
            }),
        Some((
            &swap_target,
            &no_room_placement_snapshot.build_placed_preview(&swap_target)
        )),
        "a new target with the same preview"
    );
    assert_eq!(client.find_placement_refusal(), None);

    set_placement_target(&mut client, None);
    client.update_placement_target_outcome();
    assert_eq!(
        client.get_placement_target_outcome(),
        None,
        "target cleared"
    );
}

#[test]
fn a_frame_whose_preview_equals_the_held_preview_keeps_the_held_preview() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_preview = PlacementPreview::Available(Box::new(build_test_placement_snapshot(
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    )));
    apply_frame_keeping_view(&mut client, None, Some(placement_preview.clone()));
    let held_placement_preview = Arc::clone(
        client
            .find_held_placement_preview()
            .expect("the frame's preview is held"),
    );

    apply_frame_keeping_view(&mut client, None, Some(placement_preview));

    assert!(Arc::ptr_eq(
        client
            .find_held_placement_preview()
            .expect("the frame's preview is held"),
        &held_placement_preview
    ));
}

#[test]
fn a_changed_preview_during_a_placement_drag_keeps_the_drag_and_clears_a_refused_target() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilDragEnds;
    let drag_start_position = Point { column: 2, row: 1 };
    client.begin_placement_drag(drag_start_position, true);
    assert!(client.update_placement_drag_movement(Point { column: 3, row: 1 }));
    let mut placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, source_tab_id);
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.swap_tab_sizes.clear();

    apply_frame_keeping_view(
        &mut client,
        None,
        Some(PlacementPreview::Available(Box::new(placement_snapshot))),
    );

    assert_eq!(
        client.placement_state.placement_drag,
        Some(PlacementDrag {
            start_position: drag_start_position,
            has_pointer_moved: true,
            is_insertion_drag: true,
        })
    );
    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn a_reconciled_keyboard_placement_keeps_pane_placement_mode_when_the_active_tab_changes() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let command_id = CommandId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(build_whole_tab_insertion_target(destination_tab_id));
    placement_mode.pending_placement_command_id = Some(command_id);
    let placement_frame = build_placement_frame(
        &client,
        destination_tab_id,
        &[source_tab_id, destination_tab_id],
        Some(source_pane_id),
        Some(command_id),
        None,
    );

    client.apply_render_snapshot(&placement_frame);

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert_eq!(client.active_tab_id, Some(destination_tab_id));
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.source_tab_id),
        Some(destination_tab_id)
    );
}

#[test]
fn a_frame_that_focuses_the_source_pane_and_drops_its_former_tab_keeps_pane_placement_mode() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let former_tab_id = TabId::new();
    let current_tab_id = TabId::new();
    client.set_frame_view(
        current_tab_id,
        Some(PaneId::new()),
        Some(PaneId::new()),
        vec![former_tab_id, current_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(former_tab_id),
        destination_tab_id: current_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;

    client.set_frame_view(
        current_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![current_tab_id],
    );

    assert!(client.is_placement_mode_active());
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.source_tab_id),
        Some(current_tab_id)
    );
}

#[test]
fn closing_the_former_tab_after_a_cross_tab_placement_keeps_pane_placement_mode() {
    let source_pane_id = PaneId::new();
    let former_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let command_id = CommandId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, former_tab_id, destination_tab_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(build_whole_tab_insertion_target(destination_tab_id));
    placement_mode.pending_placement_command_id = Some(command_id);
    let placement_frame = build_placement_frame(
        &client,
        destination_tab_id,
        &[former_tab_id, destination_tab_id],
        Some(source_pane_id),
        Some(command_id),
        None,
    );
    client.apply_render_snapshot(&placement_frame);

    client.set_frame_view(
        destination_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![destination_tab_id],
    );

    assert!(client.is_placement_mode_active());
    assert_eq!(
        client
            .placement_state
            .placement_mode
            .as_ref()
            .and_then(|placement_mode| placement_mode.source_tab_id),
        Some(destination_tab_id)
    );
}

#[test]
fn a_redial_keeps_an_unconfirmed_target_and_reconciles_a_confirmation_with_the_next_frame() {
    let mut client = build_test_client();
    let destination_tab_id = TabId::new();
    let unconfirmed_placement_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(PaneId::new()),
        direction: Direction::Right,
    };
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(unconfirmed_placement_target.clone()),
        pending_placement_command_id: None,
    });
    client.prepare_placement_reconciliation();
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(unconfirmed_placement_target)
    );
    assert!(!client.placement_state.needs_placement_reconciliation);

    let command_id = CommandId::new();
    let submitted_placement_target = TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(PaneId::new()),
        direction: Direction::Right,
    };
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(submitted_placement_target.clone()),
        pending_placement_command_id: Some(command_id),
    });
    client.prepare_placement_reconciliation();
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(submitted_placement_target)
    );
    assert!(client.is_pending_placement_command(command_id));
    assert!(client.placement_state.needs_placement_reconciliation);

    client.set_frame_view(TabId::new(), None, None, Vec::new());

    assert_eq!(client.get_placement_target(), None);
    assert!(!client.is_placement_confirmation_pending());
    assert!(client.is_placement_mode_active());
    assert!(!client.placement_state.needs_placement_reconciliation);
}

/// A client on a tab of the tabs `[that tab, destination_tab_id]`, focusing
/// `source_pane_id`, whose placement of `source_pane_id` into
/// `destination_tab_id` with a swap awaits the session's answer under a new
/// command id, in pane placement mode that lasts `placement_mode_lifetime`,
/// with `<C-p>` open as a key sequence.
fn build_client_with_submitted_placement(
    placement_mode_lifetime: PlacementModeLifetime,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
) -> Client {
    let mut client =
        build_client_placing_across_tabs(source_pane_id, TabId::new(), destination_tab_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(TiledPlacementTarget::Swap {
        target_pane_id: PaneId::new(),
    });
    placement_mode.pending_placement_command_id = Some(CommandId::new());
    client.placement_state.placement_mode_lifetime = placement_mode_lifetime;
    client.pending_key_sequence = Some(PendingKeySequence {
        sequence: KeySequence::from_first_and_rest(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            Vec::new(),
        ),
        ambiguity_deadline: None,
        continuation_hint_hidden_until: None,
    });
    client
}

/// Record `command_id` as the placement confirmation that `client`'s pane
/// placement mode waits for.
///
/// # Panics
/// When pane placement mode is off.
pub(crate) fn record_pending_placement_command(client: &mut Client, command_id: CommandId) {
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("placement mode is on")
        .pending_placement_command_id = Some(command_id);
}

/// The `ConfirmPlacement` that confirms `placement_target` for
/// `source_pane_id` under the command id `client`'s placement mode waits for,
/// with the tab that `client`'s held preview draws after the placement
/// ([`PlacementSnapshot::build_placed_preview`]).
///
/// # Panics
/// When no confirmation waits, `client` holds no available preview, or the
/// preview refuses `placement_target`.
pub(crate) fn build_expected_confirm_placement(
    client: &Client,
    source_pane_id: PaneId,
    placement_target: TiledPlacementTarget,
) -> PlacementInputAction {
    let placed_preview = client
        .get_placement_snapshot()
        .expect("the client holds an available preview")
        .build_placed_preview(&placement_target)
        .expect("the preview accepts the target");
    PlacementInputAction::ConfirmPlacement {
        command_id: client
            .get_pending_placement_command_id()
            .expect("the confirmation recorded a pending placement command"),
        source_pane_id,
        placement_target,
        expected_placed_tab_layout: Box::new(placed_preview.placed_tab_layout),
    }
}

#[test]
fn a_tab_hover_asks_for_no_wakeup_while_an_input_event_is_on_its_way() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let hover_started_at = Instant::now();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.update_placement_tab_hover(Some(destination_tab_id), hover_started_at);

    let read_at = client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());

    assert_eq!(
        client.compute_next_placement_tab_hover_wakeup(hover_started_at),
        None
    );

    client.get_input_read_clock().note_input_taken(read_at);

    assert_eq!(
        client.compute_next_placement_tab_hover_wakeup(hover_started_at),
        Some(PLACEMENT_TAB_HOVER_DELAY_DURATION)
    );
}

#[test]
fn hovering_another_tab_during_a_placement_drag_preserves_the_drag_and_insertion_choice() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let drag_start_position = Point { column: 2, row: 3 };
    let hover_started_at = Instant::now();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.begin_placement_drag(drag_start_position, true);
    client.update_placement_tab_hover(Some(destination_tab_id), hover_started_at);

    client.expire_placement_tab_hover(hover_started_at + PLACEMENT_TAB_HOVER_DELAY_DURATION);

    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id,
        })
    );
    assert_eq!(
        client.placement_state.placement_drag,
        Some(PlacementDrag {
            start_position: drag_start_position,
            has_pointer_moved: false,
            is_insertion_drag: true,
        })
    );
}

#[test]
fn placement_drag_stays_moved_when_pointer_returns_to_the_press_position() {
    let mut client = build_test_client();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let drag_start_position = Point { column: 2, row: 3 };
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.begin_placement_drag(drag_start_position, true);

    assert!(!client.update_placement_drag_movement(drag_start_position));
    assert!(client.update_placement_drag_movement(Point { column: 4, row: 3 }));
    assert!(client.update_placement_drag_movement(drag_start_position));
    assert_eq!(
        client.placement_state.placement_drag,
        Some(PlacementDrag {
            start_position: drag_start_position,
            has_pointer_moved: true,
            is_insertion_drag: true,
        })
    );
    client.end_placement_drag();
    assert!(!client.update_placement_drag_movement(drag_start_position));
}

#[test]
fn an_accepted_drop_ends_the_mode_a_handle_drag_opened_with_either_stay_setting() {
    for should_stay in [true, false] {
        let command_id = CommandId::new();
        let mut client = build_client_with_submitted_placement(
            PlacementModeLifetime::UntilDragEnds,
            PaneId::new(),
            TabId::new(),
        );
        client
            .client_config
            .should_stay_in_pane_placement_mode_after_placement = should_stay;
        let pending_key_sequence = client.pending_key_sequence.clone();
        record_pending_placement_command(&mut client, command_id);

        apply_frame_keeping_view(&mut client, Some(command_id), None);

        assert_eq!(
            client.placement_state.placement_mode, None,
            "stay setting {should_stay}"
        );
        assert!(!client.is_placement_confirmation_pending());
        assert!(!client.is_placement_mode_active());
        assert_eq!(client.get_active_input_mode(), LockMode::Normal);
        assert_eq!(client.pending_key_sequence, pending_key_sequence);
    }
}

#[test]
fn a_resync_ends_the_mode_a_handle_drag_opened_with_the_next_frame() {
    for is_drop_applied in [true, false] {
        let command_id = CommandId::new();
        let mut client = build_client_with_submitted_placement(
            PlacementModeLifetime::UntilDragEnds,
            PaneId::new(),
            TabId::new(),
        );
        record_pending_placement_command(&mut client, command_id);
        client.prepare_placement_reconciliation();

        apply_frame_keeping_view(&mut client, is_drop_applied.then_some(command_id), None);

        assert_eq!(
            client.placement_state.placement_mode, None,
            "drop applied {is_drop_applied}"
        );
        assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    }
}

#[test]
fn an_accepted_keyboard_placement_ends_pane_placement_mode_when_staying_is_off() {
    let command_id = CommandId::new();
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    record_pending_placement_command(&mut client, command_id);

    apply_frame_keeping_view(&mut client, Some(command_id), None);

    assert_eq!(client.placement_state.placement_mode, None);
    assert!(!client.is_placement_mode_active());
    assert!(!client.is_pane_placement_visible());
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    assert_eq!(client.pending_key_sequence, None);
}

#[test]
fn a_rejected_keyboard_placement_keeps_pane_placement_mode_when_staying_is_off() {
    let command_id = CommandId::new();
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    record_pending_placement_command(&mut client, command_id);

    assert!(client.reject_placement_command(command_id));

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn a_rejected_keyboard_placement_selects_the_whole_tab_of_the_held_preview() {
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let command_id = CommandId::new();
    let mut client =
        build_client_placing_across_tabs(source_pane_id, source_tab_id, destination_tab_id);
    let placement_snapshot =
        build_test_placement_snapshot(source_pane_id, source_tab_id, destination_tab_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(TiledPlacementTarget::Swap {
        target_pane_id: get_destination_pane_id(&placement_snapshot),
    });
    placement_mode.pending_placement_command_id = Some(command_id);
    hold_placement_snapshot(&mut client, placement_snapshot);

    assert!(client.reject_placement_command(command_id));

    assert!(client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
    assert!(!client.is_placement_confirmation_pending());
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(build_whole_tab_insertion_target(destination_tab_id))
    );
}

#[test]
fn rejected_mouse_placement_returns_to_the_base_mode() {
    let mut client = build_test_client();
    let command_id = CommandId::new();
    client.set_lock_mode(LockMode::Locked);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        }),
        pending_placement_command_id: Some(command_id),
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilDragEnds;

    assert!(client.reject_placement_command(command_id));
    assert!(!client.is_placement_mode_active());
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
    assert!(client.placement_state.placement_mode.is_none());
}

#[test]
fn a_stale_placement_rejection_does_not_clear_a_new_confirmation() {
    let mut client = build_test_client();
    let current_command_id = CommandId::new();
    let submitted_placement_target = TiledPlacementTarget::Swap {
        target_pane_id: PaneId::new(),
    };
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: Some(submitted_placement_target.clone()),
        pending_placement_command_id: Some(current_command_id),
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;

    assert!(!client.reject_placement_command(CommandId::new()));
    assert!(client.is_placement_confirmation_pending());
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(submitted_placement_target)
    );
}

/// A client that read a theme file painting the focused border `color`.
fn build_client_with_focused_border(color: RgbColor) -> Client {
    let mut client = build_test_client();
    client.load_startup_config(None, Some(build_focused_border_theme(color)), None);
    client
}

/// A frame naming `client_id` in `lock_mode` with mouse-selection mode
/// `is_mouse_selection_enabled`:
/// one empty tab, no panes.
fn build_render_snapshot(
    client_id: ClientId,
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
) -> Box<RenderSnapshot> {
    let active_tab_id = TabId::new();
    Box::new(RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id: active_tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                tab_size: TEST_VIEWPORT_SIZE,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: Vec::new(),
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id,
            focused_pane_id: None,
            active_tab_focused_pane_id: None,
            lock_mode,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled,
        },
        placement_preview: None,
    })
}

/// A theme file whose focused-border role is `color`.
fn build_focused_border_theme(color: RgbColor) -> PartialThemeConfig {
    PartialThemeConfig {
        theme_name: None,
        colors: Some(PartialColorPalette {
            border_focused: Some(color),
            ..PartialColorPalette::default()
        }),
    }
}

/// A `keybinding.kdl` binding `<C-y>` to `core:new-tab` in `normal` mode.
fn build_ctrl_y_keybindings() -> PartialKeybindingsConfig {
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        KeySequence::from(KeyChord::from_parts(
            BindingModifierFlags::CTRL,
            Key::Char('y'),
        )),
        build_core_bound_action("new-tab"),
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    PartialKeybindingsConfig {
        mode_bindings_by_name: Some(mode_bindings_by_name),
        ..PartialKeybindingsConfig::default()
    }
}

#[test]
fn a_new_client_holds_the_viewport_it_was_built_at() {
    let client = build_test_client();
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
}

#[test]
fn set_viewport_records_the_new_size() {
    let mut client = build_test_client();
    client.set_viewport_size(Size {
        column_count: 120,
        row_count: 40,
    });
    assert_eq!(
        client.get_viewport_size(),
        Size {
            column_count: 120,
            row_count: 40,
        }
    );
}

#[test]
fn the_core_pane_area_reserves_the_two_chrome_rows() {
    assert_eq!(
        compute_core_pane_area(Size {
            column_count: 80,
            row_count: 24
        }),
        PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        compute_core_pane_area(Size {
            column_count: 80,
            row_count: 1
        }),
        PaneArea::Reported(Size {
            column_count: 80,
            row_count: 0
        })
    );
}

#[test]
fn a_theme_file_recolors_the_chrome_the_next_frame_paints_with() {
    let client = build_client_with_focused_border(RgbColor::from_channels(1, 2, 3));
    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(1, 2, 3)
    );
    assert_eq!(
        client.get_client_config().theme.colors.border_focused,
        RgbColor::from_channels(1, 2, 3),
        "and the stored settings carry it too"
    );
}

#[test]
fn a_theme_files_name_reaches_the_viewers_settings() {
    let mut client = build_test_client();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            theme_name: Some("midnight".to_owned()),
            colors: Some(PartialColorPalette {
                accent: Some(RgbColor::from_channels(9, 8, 7)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

    assert_eq!(client.get_client_config().theme.theme_name, "midnight");
    assert_eq!(
        client.get_client_config().theme.colors.accent,
        RgbColor::from_channels(9, 8, 7)
    );
}

#[test]
fn a_default_config_client_paints_the_stock_colors() {
    let client = build_test_client();
    assert_eq!(*client.get_theme(), Theme::default());
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn koshi_kdls_viewer_owned_sections_reach_the_viewers_settings() {
    // `koshi.kdl` carries sections both halves read. The client folds its own
    // sections out of the same file: split direction `Down`, `7` scroll lines,
    // and wheel `Ignore`.
    let mut client = build_test_client();
    assert_eq!(
        client.get_client_config().layout.new_pane_direction,
        Direction::Right,
        "the built-in default"
    );

    client.load_startup_config(
        Some(PartialKoshiConfig {
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            mouse: Some(koshi_config::layer::PartialMouseConfig {
                can_resize_pane_border: None,
                scroll_line_count: Some(7),
                wheel_scroll: Some(WheelScroll::Ignore),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(
        client.get_client_config().layout.new_pane_direction,
        Direction::Down
    );
    assert_eq!(client.get_client_config().mouse.scroll_line_count, 7);
    assert_eq!(
        client.get_client_config().mouse.wheel_scroll,
        WheelScroll::Ignore
    );
}

#[test]
fn the_last_load_wins_when_settings_are_read_twice() {
    // The colors a frame paints with come from the newest settings.
    let mut client = build_test_client();

    client.load_startup_config(
        None,
        Some(build_focused_border_theme(RgbColor::from_channels(1, 1, 1))),
        None,
    );
    client.load_startup_config(
        None,
        Some(build_focused_border_theme(RgbColor::from_channels(2, 2, 2))),
        None,
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(2, 2, 2)
    );
}

#[test]
fn loading_no_files_at_all_leaves_the_built_in_settings() {
    // A run with no config files resolves to the same settings a fresh client
    // holds. The palette is recomputed, not accumulated.
    let mut client = build_test_client();
    let original_theme = *client.get_theme();

    let keymap_conflict_report = client.load_startup_config(None, None, None);

    assert_eq!(
        keymap_conflict_report, None,
        "no keybinding file means no report"
    );
    assert_eq!(*client.get_theme(), original_theme);
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn extreme_palette_values_survive_the_round_trip() {
    // Black and white map through the palette unchanged.
    let mut client = build_test_client();

    client.load_startup_config(
        None,
        Some(PartialThemeConfig {
            theme_name: None,
            colors: Some(PartialColorPalette {
                border_focused: Some(RgbColor::from_channels(0, 0, 0)),
                border_unfocused: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
                ramp_start: Some(RgbColor::from_channels(0, 0, 0)),
                ramp_end: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
                ..PartialColorPalette::default()
            }),
        }),
        None,
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(0, 0, 0)
    );
    assert_eq!(
        client.get_theme().unfocused_border_color,
        ratatui::style::Color::Rgb(0xff, 0xff, 0xff)
    );
    assert_eq!(client.get_theme().ramp_start, (0, 0, 0));
    assert_eq!(client.get_theme().ramp_end, (0xff, 0xff, 0xff));
}

#[test]
fn an_applied_keybinding_file_swaps_the_keymap_and_drops_an_open_sequence() {
    let mut client = build_test_client();
    let ctrl_y_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('y'),
    ));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        None,
        "nothing is bound to `<C-y>` out of the box"
    );
    // `<C-p>` opens the shipped pane group, and the client holds an open
    // sequence.
    client.resolve_key(
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
        Instant::now(),
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(KeyChord::from_parts(
            BindingModifierFlags::CTRL,
            Key::Char('p')
        )))
    );

    let keymap_conflict_report =
        client.load_startup_config(None, None, Some(build_ctrl_y_keybindings()));

    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("new-tab"))
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "held chords reached for bindings the new keymap may not have"
    );
}

#[test]
fn a_keybinding_file_can_move_the_leader_the_defaults_hang_off() {
    let mut client = build_test_client();
    let ctrl_p_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('p'),
    ));
    let alt_p_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::ALT,
        Key::Char('p'),
    ));
    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p_sequence)
            .has_longer_key_sequence
    );

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            leader: Some(Leader::Modifiers(BindingModifierFlags::ALT)),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &alt_p_sequence)
            .has_longer_key_sequence
    );
    assert!(
        !client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p_sequence)
            .has_longer_key_sequence
    );
}

#[test]
fn a_refused_keybinding_file_leaves_both_the_keymap_and_the_settings_on_the_built_ins() {
    // A file with `maximum_chord_depth` 0 is refused whole. The client keeps
    // the built-in keymap and the built-in keybinding settings.
    let mut client = build_test_client();
    let ctrl_p_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('p'),
    ));

    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client.get_client_config().keybindings,
        KeybindingsConfig::default(),
        "the refused file's settings must not describe the running keymap"
    );
    assert!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_p_sequence)
            .has_longer_key_sequence,
        "the shipped two-chord defaults still open under `<C-p>`"
    );
}

#[test]
fn a_refused_keybinding_file_leaves_an_open_sequence_alone() {
    // A refused file changes no binding. The open `<C-p>` sequence stays open.
    let mut client = build_test_client();
    client.resolve_key(
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
        Instant::now(),
    );

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![KeyChord::from_parts(
            BindingModifierFlags::CTRL,
            Key::Char('p')
        )])
    );
}

#[test]
fn a_good_keybinding_file_still_applies_after_a_refused_one() {
    // After a refused file, the next file the client reads applies.
    let mut client = build_test_client();
    let ctrl_y_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('y'),
    ));

    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );
    let keymap_conflict_report =
        client.load_startup_config(None, None, Some(build_ctrl_y_keybindings()));

    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("new-tab"))
    );
}

#[test]
fn a_keybinding_file_cannot_smuggle_colors_in_through_koshi_kdl() {
    // The client drops the theme section of `koshi.kdl`. With no theme file,
    // the client paints the built-in palette.
    let mut client = build_test_client();

    client.load_startup_config(
        Some(PartialKoshiConfig {
            theme: Some(PartialThemeConfig {
                theme_name: Some("smuggled".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::from_channels(9, 9, 9)),
                    ..PartialColorPalette::default()
                }),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    assert_eq!(*client.get_theme(), Theme::default());
    assert_eq!(
        client.get_client_config().theme,
        ClientConfig::default().theme
    );
}

#[test]
fn a_keybinding_file_without_a_pane_placement_cancel_binding_is_refused() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("pane-placement"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences: BTreeSet::from([escape_sequence.clone()]),
        },
    );
    let mut client = build_test_client();

    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    let conflict_report = keymap_conflict_report.expect("a keybinding file was given");
    assert_eq!(
        conflict_report.diagnostics,
        vec![
            koshi_config::conflict::ConflictDiagnostic::ModeCancelBindingMissing {
                mode_name: ModeName::from_text("pane-placement"),
                cancel_action_reference: ActionReference::from_core_action_name(
                    "cancel-pane-placement"
                )
                .expect("valid action name"),
            }
        ]
    );
    assert_eq!(conflict_report.get_verdict(), KeymapVerdict::Reject);
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::PanePlacement, &escape_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("cancel-pane-placement"))
    );
}

#[test]
fn a_refused_keybinding_file_keeps_the_reserved_unlock_firing() {
    // A file that binds the reserved unlock chord in locked mode is refused
    // whole. The unlock chord keeps its `unlock` action.
    let mut client = build_test_client();
    let unlock_sequence = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence
        .insert(unlock_sequence.clone(), build_core_bound_action("new-tab"));
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &unlock_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("unlock"))
    );
}

#[test]
fn a_refused_keybinding_file_still_applies_the_theme_beside_it() {
    // The three files are read in one call. A refused keybinding file leaves
    // the theme file applied.
    let mut client = build_test_client();

    client.load_startup_config(
        None,
        Some(build_focused_border_theme(RgbColor::from_channels(4, 5, 6))),
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        client.get_theme().focused_border_color,
        ratatui::style::Color::Rgb(4, 5, 6)
    );
}

#[test]
fn a_second_keybinding_file_fully_replaces_the_firsts_bindings() {
    let mut client = build_test_client();
    let ctrl_y_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('y'),
    ));

    client.load_startup_config(None, None, Some(build_ctrl_y_keybindings()));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("new-tab"))
    );

    client.load_startup_config(None, None, Some(PartialKeybindingsConfig::default()));

    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        None,
        "the first file's binding must not survive the second"
    );
}

#[test]
fn a_second_startup_load_with_no_keybinding_file_resets_the_keymap_to_the_built_ins() {
    // With no `keybinding.kdl`, the built-in keymap applies, and a binding an
    // earlier load installed stops resolving.
    let mut client = build_test_client();
    let ctrl_y_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('y'),
    ));

    client.load_startup_config(None, None, Some(build_ctrl_y_keybindings()));
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        Some(build_core_bound_action("new-tab"))
    );

    let keymap_conflict_report = client.load_startup_config(None, None, None);

    assert_eq!(
        keymap_conflict_report, None,
        "no keybinding file means no report"
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_y_sequence)
            .exact_bound_action,
        None,
        "the earlier file's binding must not outlive it"
    );
    assert_eq!(*client.get_client_config(), ClientConfig::default());
}

#[test]
fn a_low_chord_depth_drops_every_binding_longer_than_it() {
    let mut client = build_test_client();
    let long_key_sequence = KeySequence::from_first_and_rest(
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('y')),
        vec![KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('x'),
        )],
    );
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        long_key_sequence.clone(),
        build_core_bound_action("new-tab"),
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(1),
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    // Depth 1 applies — with a warning naming the unreachable binding.
    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply
    );
    // The overlong binding has no exact match, and its first chord is not a
    // prefix: the key reaches the pane.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &long_key_sequence),
        KeyMatch::default()
    );
    // The shipped two-chord defaults fall the same way.
    assert_eq!(
        client.keymap_catalog.match_sequence(
            LockMode::Normal,
            &KeySequence::from(KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Char('p')
            ))
        ),
        KeyMatch::default()
    );
    // The one-chord unlock is untouched.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(
                LockMode::Locked,
                &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
            )
            .exact_bound_action,
        Some(build_core_bound_action("unlock"))
    );
}

#[test]
fn a_keybinding_file_removes_a_default_binding_only_in_the_mode_that_declares_it() {
    let mut client = build_test_client();
    let ctrl_q_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('q'),
    ));
    let quit_action = build_core_bound_action("quit");
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_q_sequence)
            .exact_bound_action,
        Some(quit_action.clone())
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &ctrl_q_sequence)
            .exact_bound_action,
        Some(quit_action.clone())
    );

    let mut removed_key_sequences = BTreeSet::new();
    removed_key_sequences.insert(ctrl_q_sequence.clone());
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences,
        },
    );
    let keymap_conflict_report = client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );

    assert_eq!(
        keymap_conflict_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply
    );
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Normal, &ctrl_q_sequence),
        KeyMatch::default()
    );
    assert_eq!(
        *client
            .keymap_catalog
            .build_frame_hints(client.get_lock_mode(), false, false)
            .removed_key_sequences,
        BTreeSet::from([ctrl_q_sequence.clone()])
    );
    // Locked mode's own quit binding is untouched: removal is scoped to the
    // mode that declares it.
    assert_eq!(
        client
            .keymap_catalog
            .match_sequence(LockMode::Locked, &ctrl_q_sequence)
            .exact_bound_action,
        Some(quit_action)
    );
}

#[test]
fn the_focused_pane_is_floating_only_when_the_session_lists_it_as_floating() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(ClientId::new(), LockMode::Normal, false);
    render_snapshot.session_snapshot.floating_panes = vec![FloatingPane::Minimized {
        pane_id: floating_pane_id,
        pane_title: None,
    }];
    for (focused_pane_id, expected_is_focused_pane_floating) in [
        (Some(floating_pane_id), true),
        (Some(tiled_pane_id), false),
        (None, false),
    ] {
        render_snapshot.client_snapshot.focused_pane_id = focused_pane_id;

        assert_eq!(
            is_focused_pane_floating(
                &render_snapshot.session_snapshot,
                &render_snapshot.client_snapshot
            ),
            expected_is_focused_pane_floating
        );
    }
}

#[test]
fn frame_hints_follow_the_viewers_own_mode() {
    let mut client = build_test_client();
    let normal_mode_hints =
        client
            .keymap_catalog
            .build_frame_hints(client.get_lock_mode(), false, false);
    client.set_lock_mode(LockMode::Locked);
    let locked_mode_hints =
        client
            .keymap_catalog
            .build_frame_hints(client.get_lock_mode(), false, false);

    let list_sorted_hint_labels = |keymap_hints: &KeymapHints| {
        let mut hint_labels = keymap_hints
            .hint_bindings
            .iter()
            .map(|hint_binding| (hint_binding.action_label.clone(), hint_binding.is_pinned))
            .collect::<Vec<_>>();
        hint_labels.sort_unstable();
        hint_labels
    };

    assert_eq!(
        list_sorted_hint_labels(&normal_mode_hints),
        [
            "Begin Pane Placement",
            "Close Pane Tree",
            "Close Tab",
            "Dock Or Undock Pane",
            "Focus Next Floating Pane",
            "Focus Pane",
            "Focus Pane",
            "Focus Pane",
            "Focus Pane",
            "Focus Previous Floating Pane",
            "Lock",
            "Mouse Select",
            "New Pane",
            "New Stacked Pane",
            "New Tab",
            "Next Tab",
            "Previous Tab",
            "Quit",
            "Resize Pane",
            "Resize Pane",
            "Resize Pane",
            "Resize Pane",
            "Split Pane",
            "Split Pane",
            "Split Pane",
            "Split Pane",
            "Toggle Floating Pane Tray",
            "Toggle Pane Fullscreen",
        ]
        .map(|hint_label| (hint_label.to_string(), false))
    );
    assert_eq!(
        list_sorted_hint_labels(&locked_mode_hints),
        [("Mouse Select", false), ("Quit", false), ("Unlock", true)]
            .map(|(hint_label, is_pinned)| (hint_label.to_string(), is_pinned))
    );
}

#[test]
fn setting_mouse_select_moves_the_viewers_copy_both_ways() {
    // An attached client reads the mode off the frame its own connection
    // carries. The setter moves its copy.
    let mut client = build_test_client();

    client.set_mouse_selection_enabled(true);
    assert!(
        client.is_mouse_selection_enabled(),
        "the setter turned it on"
    );

    client.set_mouse_selection_enabled(false);
    assert!(
        !client.is_mouse_selection_enabled(),
        "and the next call turned it off"
    );
}

#[test]
fn a_frame_replaces_the_viewers_lock_and_mouse_select() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    client.set_mouse_selection_enabled(true);

    client.apply_render_snapshot(&build_render_snapshot(
        client.get_client_id(),
        LockMode::Normal,
        false,
    ));

    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());
}

#[test]
fn a_frame_throws_away_a_tab_strip_peek_made_on_another_tab() {
    let mut client = build_test_client();
    let peeked_tab_id = TabId::new();
    client.tabline_peek = Some(TablinePeek {
        active_tab_id: peeked_tab_id,
        first_visible_tab_index: 3,
    });

    client.apply_render_snapshot(&build_render_snapshot(
        client.get_client_id(),
        LockMode::Normal,
        false,
    ));

    assert_eq!(client.tabline_peek, None);
    assert_eq!(
        client.build_viewer_chrome(peeked_tab_id).tabline_offset,
        None,
        "switching back to the peeked tab starts the strip at its own first tab"
    );
}

#[test]
fn a_frame_keeps_a_tab_strip_peek_made_on_the_tab_it_names() {
    let mut client = build_test_client();
    let render_snapshot = build_render_snapshot(client.get_client_id(), LockMode::Normal, false);
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    client.tabline_peek = Some(TablinePeek {
        active_tab_id,
        first_visible_tab_index: 3,
    });

    client.apply_render_snapshot(&render_snapshot);

    assert_eq!(
        client.tabline_peek,
        Some(TablinePeek {
            active_tab_id,
            first_visible_tab_index: 3,
        })
    );
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).tabline_offset,
        Some(3)
    );
}

#[test]
fn the_link_status_shows_on_the_chrome_the_viewer_paints() {
    let mut client = build_test_client();
    let active_tab_id = TabId::new();
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).link_status,
        LinkStatus::Linked,
        "a joined viewer is linked"
    );

    let reconnecting_state = Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(5),
    };
    client.set_link_status(LinkStatus::Redialing(reconnecting_state));
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).link_status,
        LinkStatus::Redialing(reconnecting_state)
    );

    client.set_link_status(LinkStatus::AwaitingRestartedSession);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).link_status,
        LinkStatus::AwaitingRestartedSession
    );

    client.set_link_status(LinkStatus::Linked);
    assert_eq!(
        client.build_viewer_chrome(active_tab_id).link_status,
        LinkStatus::Linked
    );
}

#[test]
fn a_viewer_with_no_link_reads_keys_with_its_base_keymap_while_a_placement_waits() {
    let mut client = build_client_with_submitted_placement(
        PlacementModeLifetime::UntilCancelled,
        PaneId::new(),
        TabId::new(),
    );
    assert!(client.is_placement_confirmation_pending());
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);

    client.set_link_status(LinkStatus::AwaitingRestartedSession);

    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
    assert_eq!(
        client.pending_key_sequence, None,
        "the sequence open in the placement keymap closes with it"
    );
    assert_eq!(
        client.resolve_key(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('q')),
            Instant::now(),
        ),
        crate::input::KeyOutcome::Fire(build_core_bound_action("quit"))
    );
    assert!(
        client.is_placement_confirmation_pending(),
        "the placement itself still waits for its answer"
    );

    client.set_link_status(LinkStatus::Linked);
    assert_eq!(client.get_active_input_mode(), LockMode::PanePlacement);
}

#[test]
fn taking_a_new_client_id_moves_the_id_the_viewers_commands_carry() {
    let mut client = build_test_client();
    let new_client_id = ClientId::new();
    assert_ne!(client.get_client_id(), new_client_id);

    client.set_client_id(new_client_id);

    assert_eq!(client.get_client_id(), new_client_id);
}

#[test]
fn a_frame_that_does_not_name_the_pending_lock_command_leaves_the_lock_mode_the_viewer_set() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let lock_command_id = CommandId::new();
    client.apply_lock_command(lock_command_id, LockMode::Locked);

    let mut unanswered_snapshot = build_render_snapshot(client_id, LockMode::Normal, false);
    client.apply_render_snapshot(&unanswered_snapshot);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_pending_lock_command_id(), Some(lock_command_id));

    unanswered_snapshot
        .client_snapshot
        .last_applied_lock_command_id = Some(CommandId::new());
    client.apply_render_snapshot(&unanswered_snapshot);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_pending_lock_command_id(), Some(lock_command_id));
}

#[test]
fn the_frame_that_names_the_pending_lock_command_sets_the_lock_mode_and_ends_the_wait() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let lock_command_id = CommandId::new();
    client.apply_lock_command(lock_command_id, LockMode::Locked);

    // `koshi unlock --client` ran after the lock command.
    let mut answering_snapshot = build_render_snapshot(client_id, LockMode::Normal, false);
    answering_snapshot
        .client_snapshot
        .last_applied_lock_command_id = Some(lock_command_id);
    client.apply_render_snapshot(&answering_snapshot);

    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(client.get_pending_lock_command_id(), None);

    client.apply_render_snapshot(&build_render_snapshot(client_id, LockMode::Locked, false));

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
}

#[test]
fn a_viewer_waiting_for_no_lock_command_takes_each_frames_lock_mode() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();

    client.apply_render_snapshot(&build_render_snapshot(client_id, LockMode::Locked, false));
    assert_eq!(client.get_lock_mode(), LockMode::Locked);

    client.apply_render_snapshot(&build_render_snapshot(client_id, LockMode::Normal, false));
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(client.get_pending_lock_command_id(), None);
}

#[test]
fn a_rejected_paint_puts_back_the_move_mode_and_the_active_tab_focus() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let tiled_pane_id = PaneId::new();
    client.active_tab_focused_pane_id = Some(tiled_pane_id);
    client.pane_move_mode = Some(PaneMoveMode::Choice);
    let viewer_restore_point = client.build_viewer_restore_point();
    client.apply_render_snapshot(&build_render_snapshot(client_id, LockMode::Locked, false));
    assert_eq!(client.pane_move_mode, None);
    assert_eq!(client.active_tab_focused_pane_id, None);

    client.restore_viewer(viewer_restore_point);

    assert_eq!(client.pane_move_mode, Some(PaneMoveMode::Choice));
    assert_eq!(client.active_tab_focused_pane_id, Some(tiled_pane_id));
}

#[test]
fn a_rejected_paint_puts_back_the_lock_command_the_viewer_waited_for() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let lock_command_id = CommandId::new();
    client.apply_lock_command(lock_command_id, LockMode::Locked);
    let viewer_restore_point = client.build_viewer_restore_point();
    let mut answering_snapshot = build_render_snapshot(client_id, LockMode::Normal, false);
    answering_snapshot
        .client_snapshot
        .last_applied_lock_command_id = Some(lock_command_id);
    client.apply_render_snapshot(&answering_snapshot);
    assert_eq!(client.get_pending_lock_command_id(), None);

    client.restore_viewer(viewer_restore_point);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_pending_lock_command_id(), Some(lock_command_id));
}
