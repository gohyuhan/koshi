//! The attached client: join a running session and become a second window onto
//! it.
//!
//! A session runs in one of two homes, and the home is picked before the first
//! connection opens. A session on this machine is joined over its control
//! socket: the router turns the value after `koshi attach` into that session's
//! address, starting a router first when none runs, and the session's endpoint
//! file holds the token the Hello presents. The Hello and the Attach are
//! written back to back, so joining costs one round trip.
//!
//! A session on another machine is joined over TLS through the server serving
//! it: `koshi attach --remote <server> [session]` presents the secret that
//! server saved, the certificate pinned on the first connection is the only one
//! accepted, and the server resolves the session against the sessions that
//! secret reaches. The server presents that session's endpoint token and writes
//! the Hello on this client's behalf, since a caller on another machine cannot
//! read that file. From the Attach on, both homes speak the same frames on the
//! same loop.
//!
//! With no value at all, `koshi attach` offers the sessions running for this
//! user beside the sessions on every saved server that answered inside one
//! deadline, and the one picked names both the session and its home.
//!
//! Session selection and a local endpoint lookup happen before terminal
//! ownership. The terminal owner then captures the controlling terminal's
//! mode and runs one bounded capability probe in raw mode. It restores cooked
//! mode before the connection opens. After the session answers `Attached`, it
//! enters raw mode and starts mouse capture, bracketed paste, extended keyboard
//! input, resize reports, and the alternate screen. A refusal leaves the shell
//! in cooked mode. Example: a bad remote secret after a successful probe leaves
//! the shell in cooked mode and never enters the alternate screen.
//!
//! A session replacing its own process image is not a way out. The session says
//! so before it goes; this client leaves the terminal in every mode it is in
//! and comes back. On this machine it reads that session's endpoint file until
//! it names a new connection token and joins the new socket; on a server it
//! dials that server again, so the certificate, the secret and the scope are
//! checked again. A session that still holds this client's record hands it
//! back, and its first frame paints the same panes back, so the screen does not
//! flicker. A session that cannot hand the record back mints a fresh client,
//! and the viewer takes that id as its own: a restart that could not bring the
//! carried layout back holds no record, and a record whose tab closed is not
//! handed back. A session that moves this client to another session comes back
//! the same way.
//!
//! A dropped link to a session on a server is not a way out either, while
//! `remote-reconnect` is on. The viewer draws
//! `RECONNECTING (attempt 1, retry in 1s)` on its tab strip, counting the
//! seconds down as it waits, and dials that server again — after 1 second, then
//! 2, 4, 8, and 8 seconds before every dial after that — until it joins or 120
//! seconds pass. A dial the server answers with a refusal — a certificate that
//! is not the pinned one, a secret it does not admit, a session that secret does
//! not reach, or a protocol version this build does not accept — is not dialed
//! again: every identical dial gets the same answer, so the viewer stops there.
//! Each dial presents the secret the last attach
//! minted, and the session hands that attach's view back for it: the same
//! active tab, the same focused and zoomed pane of each tab, and the same
//! scroll offset of each pane. A session that no longer holds that view mints a
//! fresh client, and the viewer takes that id as its own. Everything typed
//! while the viewer had no link is dropped and never sent. On the new link the
//! viewer reads the terminal's size again and reports it. The screen keeps the
//! last frame it drew, at the size it was drawn for, until the viewer joins
//! again — a terminal resized over that stretch repaints when the first frame
//! of the new link arrives. A viewer that stops dialing restores its terminal,
//! then prints the cause it stopped on, `the session continues without you`, and
//! the command that reattaches, and exits non-zero. With `remote-reconnect` off,
//! and for a session on this machine, a dropped link ends the client.
//!
//! From there the connection carries traffic both ways. The session composes
//! this terminal's own frame — at this terminal's size and scroll position —
//! and pushes it down the event stream, which this loop paints. This terminal's
//! keys, pastes and resizes travel back up the same connection: a key the
//! viewer's keymap does not bind goes up as a key press, a binding that fires
//! is resolved against the action table here, a viewer-local action runs here,
//! a session command goes up, a paste goes up whole, and a resize goes up as
//! the new viewport and the pane area left by the built-in rows.
//! Every request leaves on its own writer thread, so a session slow to take
//! the bytes backs that thread up and never this terminal's input. The stream
//! also carries escapes a pane sent to this terminal itself rather than to the
//! picture — an OSC 52 clipboard write — which the loop writes straight to
//! stdout.
//!
//! The mouse is captured for this terminal, and the frame each paint drew is
//! kept beside the viewer, so a mouse event can be placed against the surfaces
//! that were on the screen when it happened. The viewer decides what each event
//! means at once and writes it at once: nothing mouse-shaped ever waits for the
//! session's answer, so as many rounds are on the wire as the loop decided. One
//! pass of the loop writes one round, so the events that arrived together fold
//! into it and every answer only reconciles what the session did.
//!
//! The keymap, the colors, the pane under the pointer, the tab strip's
//! position, and the sequence being typed belong to this terminal. The session
//! reports the input mode and mouse-select state in events and frames. Each pass
//! compares these values with the state shown on the screen and repaints when
//! one changes.

use std::io;
use std::io::Write;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::terminal::size;
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::input::KeyOutcome;
use crate::mouse::MouseAction;
use crate::PlacementInputAction;
use crate::{compute_core_pane_area, Client};
use koshi_config::types::BoundAction;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget,
    PanePlacementAnchor, PanePlacementTarget, PlacementRevision, SwitchSessionArgs, VisualCommand,
};
use koshi_core::geometry::{Direction, PixelCellSize, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseInput, MouseKind};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action_with_scroll_line_count, DispatchPlan};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::{IncomingEvent, SessionEvent};
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult, WireMouseAction,
};
use koshi_ipc::remote_wire::RemoteSessionRow;
use koshi_ipc::router::{
    resolve_router_endpoint_path, RouterRequestKind, RouterResult, SessionAddress, SessionSelector,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_ipc::wire::{MaybeKnown, WireName};
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, MouseFrame, PanePlacementMessage, PanePlacementPresentation,
    PlacementSnapshot, Reconnecting, RenderSnapshot, TabSnapshot, ViewerChrome,
};
use koshi_runtime::runtime::event::RuntimeEvent;

use crate::terminal;
use koshi_core::ids::parse_prefixed_uuid;
use koshi_ipc::endpoint::RESTART_WINDOW_DURATION;
use koshi_link::discovery::{self, SessionRow};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;
use koshi_link::remote_client::{
    self, DialError, ServerReference, REMOTE_RESTART_POLL_INTERVAL_DURATION,
};
use koshi_link::router_client::{self, submit_router_request};
use koshi_link::talk;

/// Rebuilding the snapshot this terminal paints from the frame the session
/// sent.
pub mod paint;

#[cfg(test)]
mod tests;

/// The size an attaching client reports when the terminal size cannot be read,
/// which is what a `koshi attach` with redirected output finds.
const FALLBACK_VIEWPORT: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The `request_id` the first request the loop sends carries. The Hello is 1
/// and the Attach is 2.
const FIRST_POST_ATTACH_REQUEST_ID: u64 = 3;

/// How long the first redial after a remote viewer's link dropped waits before
/// it dials: 1 second.
const FIRST_REDIAL_WAIT_DURATION: Duration = Duration::from_secs(1);

/// The longest one redial waits before it dials: 8 seconds.
const MAX_REDIAL_WAIT_DURATION: Duration = Duration::from_secs(8);

/// How long a remote viewer keeps redialing after its link dropped: 120
/// seconds, which is how long a session holds a detached client's view under
/// its resume token.
const REDIAL_WINDOW_DURATION: Duration = Duration::from_secs(120);

/// The number the first connection of one attachment carries. Coming back after
/// the session replaces its own process image counts up from here.
const INITIAL_CONNECTION_INDEX: u64 = 0;
/// Wakeup used for native output preparation and the first failed-frame retry.
const IMAGE_OUTPUT_STEP_DELAY_DURATION: Duration = Duration::from_millis(1);
/// Maximum wakeup after repeated native output failures.
const MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION: Duration = Duration::from_secs(1);

/// Most queued terminal and session events handled before the loop yields to
/// image output, key timeouts, and outbound requests.
const MAX_INCOMING_EVENT_COUNT_PER_PASS: usize = 16;

/// Most unread terminal and session events retained before their producer waits.
const INCOMING_QUEUE_CAPACITY: usize = MAX_INCOMING_EVENT_COUNT_PER_PASS;

/// Most image bytes copied into the cache during one attachment-loop pass.
const MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH: usize =
    koshi_ipc::frame::MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT;

/// The most decided-but-unwritten mouse actions this client holds, and the most
/// unanswered border moves it remembers. Both cap the memory a session that
/// answers slowly can make this client hold.
///
/// The number is one burst: a 200 ms round trip at 250 mouse events a second is
/// 50 events, and the busiest event — a press that clears a highlight and
/// extends a new one — decides 2 actions, so 100 actions covers that burst.
/// 256 is that burst two and a half times over, so an ordinary slow answer
/// leaves both whole and only a session that stopped answering trims.
const MAX_PENDING_MOUSE_ACTION_COUNT: usize = 256;

/// One border move already written and not yet answered.
///
/// A [`MouseAction::Resize`] names the whole distance from the drag anchor, and
/// [`Client::note_resize_applied`] moves that anchor only when an answer comes
/// back, so every move decided before that answer names cells an earlier move
/// already asked for. This entry is what takes them off: it lives from the write
/// until the answer carrying the same `request_id`, `pane_id` and `border_side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SentBorderMove {
    /// The round this move went out in.
    request_id: u64,
    /// The pane whose border the move was asked for.
    pane_id: PaneId,
    /// Which of the pane's borders the move was asked for.
    border_side: Direction,
    /// The signed cells the move asked for: `step * count`, so `1` grows the
    /// pane by one cell and `-3` shrinks it by three.
    requested_cell_delta: i32,
}

/// The viewer state a frame paint uses: its chrome, the input mode and
/// mouse-select state, the sequence the hint bar displays, and the placement
/// target and its in-pane presentation.
///
/// [`Screen`] holds the value the frame on the screen was drawn with, and
/// compares it against a fresh read at the end of every loop pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewerPaint {
    /// The pane under the pointer and where the tab strip sits, for the tab the
    /// frame on the screen shows.
    pub(crate) chrome: ViewerChrome,
    /// The active base or placement mode whose bindings the hint bar lists.
    pub(crate) lock_mode: LockMode,
    /// Whether the frame's viewer takes the mouse for text selection.
    pub(crate) is_mouse_selection_enabled: bool,
    /// The multi-chord sequence being typed, which the hint bar draws as a
    /// breadcrumb ahead of the chords that continue it. `None` when no sequence
    /// is open.
    pub(crate) pending_key_sequence: Option<KeySequence>,
    /// The checked placement target outlined in the active pane area.
    pub(crate) placement_target: Option<PanePlacementTarget>,
    /// The whole-pane visual explanation of this viewer's placement.
    pub(crate) placement_presentation: Option<PanePlacementPresentation>,
}

const PLACEMENT_ANIMATION_DURATION: Duration = Duration::from_millis(160);
const PLACEMENT_ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The viewer-owned interpolation between two placement snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementAnimation {
    /// The authoritative placement snapshot used as the animation's fixed target base.
    base_snapshot: PlacementSnapshot,
    /// The placement geometry visible when this animation began.
    from_snapshot: PlacementSnapshot,
    /// The placement geometry this animation reaches.
    to_snapshot: PlacementSnapshot,
    /// The target whose outline belongs to `to_snapshot`.
    placement_target: Option<PanePlacementTarget>,
    /// The monotonic time at which this interpolation began.
    started_at: Instant,
}

impl PlacementAnimation {
    /// Return the interpolated preview at `current_time`.
    fn build_placement_snapshot(&self, current_time: Instant) -> PlacementSnapshot {
        terminal::interpolate_placement_snapshot(
            &self.from_snapshot,
            &self.to_snapshot,
            compute_placement_animation_progress(self.started_at, current_time),
        )
    }
}

/// The viewer-owned slide from the tab this viewer drew to the tab after
/// another viewer's accepted pane placement.
#[derive(Debug)]
struct CommittedPlacementAnimation {
    /// The active tab as drawn when the accepted placement's frame arrived.
    from_tab_snapshot: TabSnapshot,
    /// The monotonic time at which this slide began.
    started_at: Instant,
}

/// Return the bounded ease-out progress at `current_time` of a placement
/// animation that began at `started_at`: `0.0` at the start, `1.0` after
/// `PLACEMENT_ANIMATION_DURATION`.
fn compute_placement_animation_progress(started_at: Instant, current_time: Instant) -> f32 {
    let elapsed_duration = current_time.saturating_duration_since(started_at);
    let linear_progress =
        (elapsed_duration.as_secs_f32() / PLACEMENT_ANIMATION_DURATION.as_secs_f32()).min(1.0);
    1.0 - (1.0 - linear_progress).powi(3)
}

/// Return whether a committed placement can slide from `from_tab_snapshot` to
/// `to_tab_snapshot`: both show the same tab at the same size and layout mode,
/// and neither has every pane suppressed.
fn can_slide_between_tab_snapshots(
    from_tab_snapshot: &TabSnapshot,
    to_tab_snapshot: &TabSnapshot,
) -> bool {
    from_tab_snapshot.tab_id == to_tab_snapshot.tab_id
        && from_tab_snapshot.tab_size == to_tab_snapshot.tab_size
        && from_tab_snapshot.layout_mode == to_tab_snapshot.layout_mode
        && !from_tab_snapshot.is_every_pane_suppressed
        && !to_tab_snapshot.is_every_pane_suppressed
}

impl ViewerPaint {
    /// Read what `client` contributes to a paint of `render_snapshot` that
    /// shows `displayed_active_tab_id`. A tab-strip peek made on any other tab
    /// does not apply. The placement message names tabs from
    /// `render_snapshot`'s tab list.
    pub(crate) fn from_client(
        client: &Client,
        displayed_active_tab_id: TabId,
        render_snapshot: &RenderSnapshot,
    ) -> Self {
        ViewerPaint {
            chrome: client.build_viewer_chrome(displayed_active_tab_id),
            lock_mode: client.get_active_input_mode(),
            is_mouse_selection_enabled: client.is_mouse_selection_enabled(),
            pending_key_sequence: client.get_pending_key_sequence().cloned(),
            placement_target: client.get_placement_target(),
            placement_presentation: build_pane_placement_presentation(client, render_snapshot),
        }
    }
}

/// Describe the proposed positions inside the affected panes.
fn build_pane_placement_presentation(
    client: &Client,
    render_snapshot: &RenderSnapshot,
) -> Option<PanePlacementPresentation> {
    if !client.is_pane_placement_visible() {
        return None;
    }
    let source_pane_id = client.get_placement_source_pane_id()?;
    let destination_tab_name = client
        .get_placement_destination_tab_id()
        .and_then(|destination_tab_id| {
            render_snapshot
                .session_snapshot
                .tabs_metadata
                .iter()
                .find(|tab_metadata| tab_metadata.tab_id == destination_tab_id)
                .map(|tab_metadata| tab_metadata.tab_name.as_str())
        })
        .unwrap_or("this tab");
    let moving_pane_selection_text = if client.is_placement_preview_loading() {
        format!("Moving pane: loading {destination_tab_name}")
    } else {
        "Moving pane: choose a destination".to_string()
    };
    let source_selection_message = PanePlacementMessage {
        full_text: moving_pane_selection_text,
        compact_text: "Moving pane",
        detail_text: Some("Esc to cancel".to_string()),
    };
    let (Some(placement_snapshot), Some(placement_target)) = (
        client.get_placement_snapshot(),
        client.get_placement_target(),
    ) else {
        return Some(PanePlacementPresentation {
            source_pane_id,
            target_pane_ids: Vec::new(),
            source_message: source_selection_message,
            target_message: None,
        });
    };
    let is_cross_tab = placement_snapshot.source_tab_id != placement_snapshot.destination_tab_id;
    let confirmation_hint = if client.is_placement_confirmation_pending() {
        "Waiting for session"
    } else if client.is_placement_drag_active() {
        "Release to place · Esc to cancel"
    } else {
        "Enter to place · Esc to cancel"
    };
    let (mut target_pane_ids, moving_pane_text, moving_pane_compact_text, target_message) =
        match &placement_target {
            PanePlacementTarget::Swap { target_pane_id } => (
                vec![*target_pane_id],
                "Moving pane will land here".to_string(),
                "Moving pane",
                PanePlacementMessage {
                    full_text: "Other pane will move here".to_string(),
                    compact_text: "Other pane",
                    detail_text: Some("Into the moving pane's old place".to_string()),
                },
            ),
            PanePlacementTarget::Split {
                anchor, direction, ..
            } => {
                let (target_pane_ids, affected_pane_subject) = match anchor {
                    PanePlacementAnchor::Pane(target_pane_id) => {
                        (vec![*target_pane_id], "Other pane")
                    }
                    PanePlacementAnchor::Group(target_pane_ids) => {
                        (target_pane_ids.clone(), "Other panes")
                    }
                    PanePlacementAnchor::Tab => {
                        let destination_tab_snapshot = if is_cross_tab {
                            placement_snapshot.destination_tab_snapshot.as_ref()
                        } else {
                            Some(&placement_snapshot.source_tab_snapshot)
                        };
                        (
                            destination_tab_snapshot
                                .map(|destination_tab_snapshot| {
                                    destination_tab_snapshot
                                        .pane_slots
                                        .iter()
                                        .map(|pane_slot| pane_slot.pane_id)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            "Other panes",
                        )
                    }
                };
                let direction_label = format_placement_direction_label(*direction);
                (
                    target_pane_ids,
                    format!("Moving pane will insert {direction_label}"),
                    "Moving pane",
                    PanePlacementMessage {
                        full_text: format!(
                            "{affected_pane_subject} will make room {direction_label}"
                        ),
                        compact_text: affected_pane_subject,
                        detail_text: None,
                    },
                )
            }
        };
    target_pane_ids.retain(|target_pane_id| *target_pane_id != source_pane_id);
    let moving_pane_detail_text = if client.is_placement_confirmation_pending() {
        confirmation_hint.to_string()
    } else if is_cross_tab {
        match placement_target {
            PanePlacementTarget::Swap { .. } => format!(
                "Other pane takes the old place in {}",
                placement_snapshot.source_tab_snapshot.tab_name
            ),
            PanePlacementTarget::Split { .. } => format!("In {destination_tab_name}"),
        }
    } else {
        confirmation_hint.to_string()
    };
    Some(PanePlacementPresentation {
        source_pane_id,
        target_message: (!target_pane_ids.is_empty()).then_some(target_message),
        target_pane_ids,
        source_message: PanePlacementMessage {
            full_text: if client.is_placement_confirmation_pending() {
                "Placing this pane…".to_string()
            } else {
                moving_pane_text
            },
            compact_text: if client.is_placement_confirmation_pending() {
                "Placing…"
            } else {
                moving_pane_compact_text
            },
            detail_text: Some(moving_pane_detail_text),
        },
    })
}

/// Return the text for one placement insertion direction.
fn format_placement_direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Up => "above",
        Direction::Down => "below",
        Direction::Left => "to the left",
        Direction::Right => "to the right",
    }
}

/// This terminal's screen: the frame drawn on it, the committed region solve,
/// and what the viewer contributed to that frame.
///
/// Every draw goes through here. `draw` paints a session frame and returns its
/// mouse view. `refresh` commits a pending frame or redraws when viewer paint or
/// placement snapshots change, or while another viewer's accepted placement
/// slides. Enter or mouse release changes the in-pane message to
/// `Placing this pane…` until the session rejects the command, or commits
/// it and a painted frame carries the new placement revision.
struct Screen<B: Backend> {
    /// The ratatui terminal the renderer paints into.
    terminal: Terminal<B>,
    /// The window title used for the last title-write comparison.
    last_window_title: String,
    /// The cursor style used for the last cursor-style-write comparison.
    last_cursor_style: Option<CursorStyle>,
    /// Whether this viewer requested Shift mouse reports from the outer terminal.
    is_shift_mouse_capture_requested: bool,
    /// The snapshot last drawn, kept so a viewer-only change can draw it
    /// again without re-reading the frame. Its grids travel behind `Arc`s, so
    /// retaining it does not copy cell data. `None` until the first draw.
    last_snapshot: Option<RenderSnapshot>,
    /// The retained read-only placement snapshot, if one is accepted.
    placement_snapshot: Option<PlacementSnapshot>,
    /// The placement snapshot used by the last successful paint.
    shown_placement_snapshot: Option<PlacementSnapshot>,
    /// The interpolated placement snapshot used by the last successful paint.
    shown_placement_render_snapshot: Option<PlacementSnapshot>,
    /// The viewer-owned placement interpolation, if one is active.
    placement_animation: Option<PlacementAnimation>,
    /// The active tab drawn by the last successful paint, with any placement
    /// preview or committed-placement slide applied. `None` until the first draw.
    shown_tab_snapshot: Option<TabSnapshot>,
    /// The tabs named by another viewer's accepted pane placement, waiting for
    /// the next session frame.
    committed_placement_tab_ids: Vec<TabId>,
    /// The slide after another viewer's accepted pane placement, if one is active.
    committed_placement_animation: Option<CommittedPlacementAnimation>,
    /// The region solve and input revision committed with the frame on the
    /// screen. It starts with the compiled-in two-row solve.
    committed_regions: CommittedRegions,
    /// What the viewer contributed to the frame on the screen. `None` until the
    /// first draw.
    shown_viewer_paint: Option<ViewerPaint>,
    /// The newest complete snapshot waiting for native image preparation.
    pending_snapshot: Option<RenderSnapshot>,
    /// The image protocol capability of the outer terminal.
    graphics_support: terminal::GraphicsSupport,
    /// Connection-local native image output state.
    image_output_state: terminal::ImageOutputState,
    /// The delay before retrying a failed native frame commit.
    native_retry_delay: Duration,
    /// The next time a failed native frame commit may run again.
    native_retry_at: Option<Instant>,
    /// The cell dimensions reported by the outer terminal.
    cell_size: Option<PixelCellSize>,
}

impl<B: Backend> Screen<B> {
    /// A screen that has drawn nothing yet, with its image protocol capability.
    fn with_graphics_support(
        terminal: Terminal<B>,
        viewport_size: Size,
        graphics_support: terminal::GraphicsSupport,
        cell_size: Option<PixelCellSize>,
    ) -> Self {
        Screen {
            terminal,
            last_window_title: String::new(),
            last_cursor_style: None,
            is_shift_mouse_capture_requested: false,
            last_snapshot: None,
            placement_snapshot: None,
            shown_placement_snapshot: None,
            shown_placement_render_snapshot: None,
            placement_animation: None,
            shown_tab_snapshot: None,
            committed_placement_tab_ids: Vec::new(),
            committed_placement_animation: None,
            committed_regions: CommittedRegions::build_core(viewport_size, 0),
            shown_viewer_paint: None,
            pending_snapshot: None,
            graphics_support,
            image_output_state: terminal::ImageOutputState::from_output_kind(
                terminal::ImageOutputKind::from_support(graphics_support),
            ),
            native_retry_delay: IMAGE_OUTPUT_STEP_DELAY_DURATION,
            native_retry_at: None,
            cell_size,
        }
    }

    /// Draw `snapshot`, built from one frame the session sent, and hand back the
    /// frame a mouse event is placed against. Returns `None` when the terminal
    /// rejects the paint.
    ///
    /// It applies the snapshot to `client` and paints from the result. When the
    /// terminal rejects the paint, `client` returns to its state before the
    /// frame: a rejected locked frame leaves the viewer unlocked.
    ///
    /// The returned [`MouseFrame`] holds the committed region solve, where the
    /// surfaces sit, and the per-pane scroll and mouse fields. That is what the
    /// next mouse event is answered from.
    fn draw_snapshot(
        &mut self,
        client: &mut Client,
        snapshot: RenderSnapshot,
    ) -> Option<MouseFrame> {
        self.pending_snapshot = Some(snapshot);
        self.commit_pending_snapshot(client, Instant::now())
    }

    /// Keep the outer terminal's Shift mouse reporting request aligned with
    /// this viewer's placement mode, including changes made by session frames.
    fn update_shift_mouse_capture<W: Write>(
        &mut self,
        client: &Client,
        writer: &mut W,
    ) -> io::Result<()> {
        terminal::update_shift_mouse_capture(
            writer,
            client.is_placement_mode_active(),
            &mut self.is_shift_mouse_capture_requested,
        )
    }

    /// Apply the pending frame to `client` and paint it from the result. A
    /// rejected paint puts `client` back as it was before the frame and keeps
    /// the frame pending. Before a native image retry is due, it paints nothing
    /// and changes nothing.
    fn commit_pending_snapshot(
        &mut self,
        client: &mut Client,
        current_time: Instant,
    ) -> Option<MouseFrame> {
        let is_native_image_output = self.image_output_state.get_output_kind().is_some();
        if is_native_image_output
            && self
                .native_retry_at
                .is_some_and(|retry_time| current_time < retry_time)
        {
            return None;
        }
        let viewer_restore_point = client.build_viewer_restore_point();
        client.apply_render_snapshot(self.pending_snapshot.as_ref()?);
        self.drop_stale_placement_snapshot(
            client,
            get_frame_placement_revision(self.pending_snapshot.as_ref()?),
        );
        let viewport_size = self
            .pending_snapshot
            .as_ref()?
            .client_snapshot
            .viewport_size;
        let committed_regions = self.compute_committed_regions(viewport_size);
        let placement_display_snapshot = self.update_placement_animation(
            client,
            client.get_placement_target().as_ref(),
            current_time,
        );
        let committed_placement_tab_ids = std::mem::take(&mut self.committed_placement_tab_ids);
        let committed_placement_tab_snapshot = self.update_committed_placement_animation(
            client,
            &committed_placement_tab_ids,
            placement_display_snapshot.is_some(),
            current_time,
        );
        let snapshot = self.pending_snapshot.as_ref()?;
        let placement_render_snapshot = placement_display_snapshot
            .as_ref()
            .or(self.placement_snapshot.as_ref());
        let displayed_render_snapshot = build_displayed_render_snapshot(
            snapshot,
            placement_render_snapshot,
            committed_placement_tab_snapshot,
        );
        let displayed_snapshot = displayed_render_snapshot.as_ref().unwrap_or(snapshot);
        let frame_paint = ViewerPaint::from_client(
            client,
            displayed_snapshot.client_snapshot.active_tab_id,
            snapshot,
        );
        match terminal::paint_frame_with_displayed_snapshot(
            &mut self.terminal,
            client,
            displayed_snapshot,
            &committed_regions,
            &frame_paint,
            self.graphics_support.get_image_render_mode(),
            &mut self.image_output_state,
            self.cell_size,
            &mut self.last_window_title,
            &mut self.last_cursor_style,
            self.placement_snapshot.as_ref(),
            placement_display_snapshot.as_ref(),
        ) {
            Ok(true) => {
                if is_native_image_output {
                    self.native_retry_delay = IMAGE_OUTPUT_STEP_DELAY_DURATION;
                    self.native_retry_at = None;
                }
            }
            Ok(false) => {
                client.restore_viewer(viewer_restore_point);
                return None;
            }
            Err(paint_error) => {
                client.restore_viewer(viewer_restore_point);
                warn_paint_error(paint_error);
                if is_native_image_output {
                    self.native_retry_delay = self
                        .native_retry_delay
                        .saturating_mul(2)
                        .min(MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION);
                    self.native_retry_at = Some(current_time + self.native_retry_delay);
                }
                return None;
            }
        }
        let snapshot = self
            .pending_snapshot
            .take()
            .expect("the committed snapshot is pending");
        let displayed_snapshot = displayed_render_snapshot.as_ref().unwrap_or(&snapshot);
        self.committed_regions = committed_regions.clone();
        self.shown_viewer_paint = Some(frame_paint);
        self.shown_placement_snapshot = self.placement_snapshot.clone();
        self.shown_placement_render_snapshot = placement_render_snapshot.cloned();
        self.shown_tab_snapshot = Some(
            displayed_snapshot
                .session_snapshot
                .active_tab_snapshot
                .clone(),
        );
        let mouse_frame_snapshot = if placement_render_snapshot.is_some() {
            displayed_snapshot
        } else {
            &snapshot
        };
        let mouse_frame = MouseFrame::from_snapshot(mouse_frame_snapshot, committed_regions);
        self.last_snapshot = Some(snapshot);
        Some(mouse_frame)
    }

    /// Commit a pending frame or redraw the last frame when viewer paint or a
    /// placement snapshot changes, or while another viewer's accepted placement
    /// slides. Enter or mouse release changes the in-pane message to
    /// `Placing this pane…` until the session rejects the command, or commits
    /// it and a painted frame carries the new placement revision.
    ///
    /// `active_tab_id` is `Some` after the first frame has been drawn. A viewer
    /// with no changed local state draws nothing. A viewer whose viewport no
    /// longer matches the committed region solve draws nothing until the next
    /// session frame.
    fn refresh(&mut self, client: &mut Client, active_tab_id: Option<TabId>) -> Option<MouseFrame> {
        self.refresh_at(client, active_tab_id, Instant::now())
    }

    fn refresh_at(
        &mut self,
        client: &mut Client,
        active_tab_id: Option<TabId>,
        current_time: Instant,
    ) -> Option<MouseFrame> {
        self.image_output_state.poll();
        if self.pending_snapshot.is_some() {
            return self.commit_pending_snapshot(client, current_time);
        }
        active_tab_id?;
        if client.get_viewport_size() != self.committed_regions.viewport_size {
            return None;
        }
        self.drop_stale_placement_snapshot(
            client,
            get_frame_placement_revision(self.last_snapshot.as_ref()?),
        );
        let placement_display_snapshot = self.update_placement_animation(
            client,
            client.get_placement_target().as_ref(),
            current_time,
        );
        let committed_placement_tab_snapshot = self.update_committed_placement_animation(
            client,
            &[],
            placement_display_snapshot.is_some(),
            current_time,
        );
        let snapshot = self.last_snapshot.as_ref()?;
        let placement_render_snapshot = placement_display_snapshot
            .as_ref()
            .or(self.placement_snapshot.as_ref());
        let displayed_active_tab_id = placement_render_snapshot
            .and_then(terminal::find_displayed_placement_tab_snapshot)
            .map_or(
                snapshot.client_snapshot.active_tab_id,
                |displayed_tab_snapshot| displayed_tab_snapshot.tab_snapshot.tab_id,
            );
        let viewer_paint = ViewerPaint::from_client(client, displayed_active_tab_id, snapshot);
        let is_committed_tab_shown = placement_render_snapshot.is_some()
            || self.shown_tab_snapshot.as_ref()
                == Some(
                    committed_placement_tab_snapshot
                        .as_ref()
                        .unwrap_or(&snapshot.session_snapshot.active_tab_snapshot),
                );
        if self.shown_viewer_paint.as_ref() == Some(&viewer_paint)
            && self.shown_placement_snapshot == self.placement_snapshot
            && self.shown_placement_render_snapshot.as_ref() == placement_render_snapshot
            && is_committed_tab_shown
        {
            return None;
        }
        let displayed_render_snapshot = build_displayed_render_snapshot(
            snapshot,
            placement_render_snapshot,
            committed_placement_tab_snapshot,
        );
        let displayed_snapshot = displayed_render_snapshot.as_ref().unwrap_or(snapshot);
        if !is_frame_committed(terminal::paint_frame_with_displayed_snapshot(
            &mut self.terminal,
            client,
            displayed_snapshot,
            &self.committed_regions,
            &viewer_paint,
            self.graphics_support.get_image_render_mode(),
            &mut self.image_output_state,
            self.cell_size,
            &mut self.last_window_title,
            &mut self.last_cursor_style,
            self.placement_snapshot.as_ref(),
            placement_display_snapshot.as_ref(),
        )) {
            return None;
        }
        self.shown_viewer_paint = Some(viewer_paint);
        self.shown_placement_snapshot = self.placement_snapshot.clone();
        self.shown_placement_render_snapshot = placement_render_snapshot.cloned();
        self.shown_tab_snapshot = Some(
            displayed_snapshot
                .session_snapshot
                .active_tab_snapshot
                .clone(),
        );
        let mouse_frame_snapshot = if placement_render_snapshot.is_some() {
            displayed_snapshot
        } else {
            snapshot
        };
        Some(MouseFrame::from_snapshot(
            mouse_frame_snapshot,
            self.committed_regions.clone(),
        ))
    }

    /// Update the local placement interpolation and return its current frame.
    fn update_placement_animation(
        &mut self,
        client: &Client,
        placement_target: Option<&PanePlacementTarget>,
        current_time: Instant,
    ) -> Option<PlacementSnapshot> {
        let base_snapshot = self
            .placement_snapshot
            .as_ref()
            .or_else(|| {
                self.placement_animation
                    .as_ref()
                    .map(|animation| &animation.base_snapshot)
            })
            .cloned();
        let Some(base_snapshot) = base_snapshot else {
            self.placement_animation = None;
            return None;
        };
        let desired_snapshot = if self.placement_snapshot.is_some() {
            terminal::build_placement_preview_snapshot(&base_snapshot, placement_target)
        } else {
            base_snapshot.clone()
        };
        let is_already_showing_desired_snapshot = self.shown_placement_snapshot.as_ref()
            == self.placement_snapshot.as_ref()
            && self.shown_placement_render_snapshot.as_ref() == Some(&desired_snapshot);
        if self.placement_animation.is_none() && is_already_showing_desired_snapshot {
            return self
                .placement_snapshot
                .is_some()
                .then_some(desired_snapshot);
        }
        if client.get_client_config().should_reduce_motion {
            self.placement_animation = None;
            return self
                .placement_snapshot
                .is_some()
                .then_some(desired_snapshot);
        }
        let should_start_animation = self.placement_animation.as_ref().is_none_or(|animation| {
            animation.base_snapshot != base_snapshot
                || animation.to_snapshot != desired_snapshot
                || animation.placement_target.as_ref() != placement_target
        });
        if should_start_animation {
            let from_snapshot = self
                .placement_animation
                .as_ref()
                .map(|animation| animation.build_placement_snapshot(current_time))
                .or_else(|| {
                    (self.shown_placement_snapshot.as_ref() == Some(&base_snapshot))
                        .then(|| self.shown_placement_render_snapshot.clone())
                        .flatten()
                })
                .unwrap_or_else(|| base_snapshot.clone());
            if from_snapshot == desired_snapshot {
                self.placement_animation = None;
                return self
                    .placement_snapshot
                    .is_some()
                    .then_some(desired_snapshot);
            }
            self.placement_animation = Some(PlacementAnimation {
                base_snapshot,
                from_snapshot,
                to_snapshot: desired_snapshot.clone(),
                placement_target: placement_target.cloned(),
                started_at: current_time,
            });
        }
        let placement_animation = self
            .placement_animation
            .as_ref()
            .expect("placement animation starts before it is read");
        if compute_placement_animation_progress(placement_animation.started_at, current_time) >= 1.0
        {
            self.placement_animation = None;
            return self
                .placement_snapshot
                .is_some()
                .then_some(desired_snapshot);
        }
        Some(placement_animation.build_placement_snapshot(current_time))
    }

    /// Start or advance the slide after another viewer's accepted pane
    /// placement, and return the active tab it draws at `current_time`.
    ///
    /// The committed tab is the active tab of the pending frame, else of the
    /// last drawn frame. A slide starts from `shown_tab_snapshot` when
    /// `committed_placement_tab_ids` holds the committed tab's id, the two tabs
    /// pass `can_slide_between_tab_snapshots`, and their pane slots differ. A
    /// slide already running continues from its own start. Returns `None` and
    /// ends the slide when `is_placement_shown` is `true`, reduced motion is on,
    /// `PLACEMENT_ANIMATION_DURATION` has passed, or the committed tab no longer
    /// passes `can_slide_between_tab_snapshots` with the slide's first tab.
    fn update_committed_placement_animation(
        &mut self,
        client: &Client,
        committed_placement_tab_ids: &[TabId],
        is_placement_shown: bool,
        current_time: Instant,
    ) -> Option<TabSnapshot> {
        let committed_tab_snapshot = &self
            .pending_snapshot
            .as_ref()
            .or(self.last_snapshot.as_ref())?
            .session_snapshot
            .active_tab_snapshot;
        if is_placement_shown || client.get_client_config().should_reduce_motion {
            self.committed_placement_animation = None;
            return None;
        }
        let shown_tab_snapshot = self
            .shown_tab_snapshot
            .as_ref()
            .filter(|shown_tab_snapshot| {
                committed_placement_tab_ids.contains(&committed_tab_snapshot.tab_id)
                    && can_slide_between_tab_snapshots(shown_tab_snapshot, committed_tab_snapshot)
                    && shown_tab_snapshot.pane_slots != committed_tab_snapshot.pane_slots
            });
        if let Some(shown_tab_snapshot) = shown_tab_snapshot {
            self.committed_placement_animation = Some(CommittedPlacementAnimation {
                from_tab_snapshot: shown_tab_snapshot.clone(),
                started_at: current_time,
            });
        }
        let committed_placement_animation = self.committed_placement_animation.as_ref()?;
        let progress = compute_placement_animation_progress(
            committed_placement_animation.started_at,
            current_time,
        );
        if progress >= 1.0
            || !can_slide_between_tab_snapshots(
                &committed_placement_animation.from_tab_snapshot,
                committed_tab_snapshot,
            )
        {
            self.committed_placement_animation = None;
            return None;
        }
        Some(terminal::interpolate_committed_placement_tab_snapshot(
            &committed_placement_animation.from_tab_snapshot,
            committed_tab_snapshot,
            progress,
        ))
    }

    /// Record the tabs another viewer's accepted pane placement changed. The
    /// next session frame whose active tab is one of `tab_ids` slides from the
    /// tab this screen drew.
    fn note_committed_placement(&mut self, tab_ids: [TabId; 2]) {
        self.committed_placement_tab_ids.extend(tab_ids);
    }

    /// Drop the placement preview this screen holds when `client` holds none,
    /// or when the preview was read at a session or client revision other than
    /// `frame_placement_revision`. An outdated preview also drops its
    /// animation and the preview recorded as shown.
    fn drop_stale_placement_snapshot(
        &mut self,
        client: &Client,
        frame_placement_revision: PlacementRevision,
    ) {
        if client.get_placement_snapshot().is_none() {
            self.placement_snapshot = None;
        }
        let is_placement_snapshot_outdated =
            self.placement_snapshot
                .as_ref()
                .is_some_and(|placement_snapshot| {
                    placement_snapshot.session_placement_revision
                        != frame_placement_revision.session_revision
                        || placement_snapshot.client_placement_revision
                            != frame_placement_revision.client_revision
                });
        if is_placement_snapshot_outdated {
            self.placement_snapshot = None;
            self.placement_animation = None;
            self.shown_placement_snapshot = None;
            self.shown_placement_render_snapshot = None;
        }
    }

    fn compute_next_image_wakeup_at(&self, current_time: Instant) -> Option<Duration> {
        if self.image_output_state.is_work_pending() {
            return Some(IMAGE_OUTPUT_STEP_DELAY_DURATION);
        }
        let is_native_frame_pending =
            self.pending_snapshot.is_some() && self.image_output_state.get_output_kind().is_some();
        let retry_delay = self
            .native_retry_at
            .map_or(self.native_retry_delay, |retry_time| {
                retry_time.saturating_duration_since(current_time)
            });
        is_native_frame_pending.then_some(retry_delay)
    }

    /// Return the next redraw interval for an active placement preview
    /// interpolation or committed-placement slide.
    fn compute_next_placement_animation_wakeup_at(
        &self,
        current_time: Instant,
    ) -> Option<Duration> {
        let animation_started_at = self
            .placement_animation
            .as_ref()
            .map(|placement_animation| placement_animation.started_at)
            .into_iter()
            .chain(
                self.committed_placement_animation
                    .as_ref()
                    .map(|committed_placement_animation| committed_placement_animation.started_at),
            )
            .min()?;
        let animation_remaining = PLACEMENT_ANIMATION_DURATION
            .saturating_sub(current_time.saturating_duration_since(animation_started_at));
        Some(animation_remaining.min(PLACEMENT_ANIMATION_FRAME_INTERVAL))
    }

    /// Update the pixel dimensions used to scale native image output.
    fn set_cell_size(&mut self, cell_size: Option<PixelCellSize>) {
        if self.cell_size == cell_size {
            return;
        }
        self.cell_size = cell_size;
        if self
            .image_output_state
            .get_output_kind()
            .is_some_and(|output_kind| {
                matches!(output_kind, terminal::ImageOutputKind::Iterm)
                    || terminal::ImageOutputKind::is_sixel(output_kind)
            })
        {
            self.image_output_state.reset_connection();
            self.native_retry_delay = IMAGE_OUTPUT_STEP_DELAY_DURATION;
            self.native_retry_at = None;
            if self.pending_snapshot.is_none() {
                self.pending_snapshot.clone_from(&self.last_snapshot);
            }
        }
    }

    /// Replace the placement snapshot shown with the next refresh.
    fn set_placement_snapshot(&mut self, placement_snapshot: Option<PlacementSnapshot>) {
        self.placement_snapshot = placement_snapshot;
    }

    /// Reset native output state after the session image cache or connection changes.
    fn reset_connection(&mut self) {
        self.image_output_state.reset_connection();
        self.native_retry_delay = IMAGE_OUTPUT_STEP_DELAY_DURATION;
        self.native_retry_at = None;
        self.placement_animation = None;
        self.shown_placement_render_snapshot = None;
        self.committed_placement_tab_ids.clear();
        self.committed_placement_animation = None;
    }

    /// Select the compiled-in region solve for a painted frame's viewport.
    ///
    /// A changed frame viewport is a new region input. The revision increases
    /// only when that input changes, so another frame with the same viewport
    /// keeps the same revision.
    fn compute_committed_regions(&self, viewport_size: Size) -> CommittedRegions {
        let region_input_revision = if viewport_size == self.committed_regions.viewport_size {
            self.committed_regions.region_input_revision
        } else {
            self.committed_regions
                .region_input_revision
                .saturating_add(1)
        };
        CommittedRegions::build_core(viewport_size, region_input_revision)
    }
}

/// Return the session and client placement revisions `frame_snapshot` carries.
fn get_frame_placement_revision(frame_snapshot: &RenderSnapshot) -> PlacementRevision {
    PlacementRevision {
        session_revision: frame_snapshot.session_snapshot.session_revision,
        client_revision: frame_snapshot.client_snapshot.client_revision,
    }
}

/// Return the frame painted in place of `snapshot`:
///
/// - with `placement_render_snapshot`, `snapshot` showing that preview's tab;
/// - else with `committed_placement_tab_snapshot`, `snapshot` with that slide
///   frame as its active tab;
/// - else `None`, and `snapshot` is painted as it is.
fn build_displayed_render_snapshot(
    snapshot: &RenderSnapshot,
    placement_render_snapshot: Option<&PlacementSnapshot>,
    committed_placement_tab_snapshot: Option<TabSnapshot>,
) -> Option<RenderSnapshot> {
    match placement_render_snapshot {
        Some(placement_snapshot) => {
            terminal::build_placement_render_snapshot(snapshot, placement_snapshot)
        }
        None => committed_placement_tab_snapshot.map(|animated_tab_snapshot| {
            terminal::build_committed_placement_render_snapshot(snapshot, animated_tab_snapshot)
        }),
    }
}

fn warn_paint_error<BackendError: std::fmt::Debug>(
    paint_error: terminal::PaintError<BackendError>,
) {
    match paint_error {
        terminal::PaintError::Backend(backend_error) => {
            tracing::warn!(?backend_error, "could not paint the frame")
        }
        terminal::PaintError::Image(image_error) => {
            tracing::warn!(%image_error, "could not paint terminal images")
        }
    }
}

/// Report a paint error and return whether the complete frame was committed.
fn is_frame_committed<BackendError: std::fmt::Debug>(
    paint_result: Result<bool, terminal::PaintError<BackendError>>,
) -> bool {
    match paint_result {
        Ok(committed) => committed,
        Err(paint_error) => {
            warn_paint_error(paint_error);
            false
        }
    }
}

/// How an attached client's event stream ended.
#[derive(Debug)]
enum AttachmentEnding {
    /// The server detached this client. The session keeps running.
    Detached,
    /// The session shut down and said so before closing.
    SessionEnded,
    /// The connection broke: the session server is gone.
    ConnectionDied,
    /// This terminal went away while the session kept running.
    TerminalGone,
    /// The session moved this client to the session named here.
    SwitchSession(SessionId),
    /// The session is replacing its own process image. The loop waits for the
    /// session's new socket and attaches again on it. A loop that cannot reach
    /// the session's new image ends here, and the report names the restart and
    /// the attach command.
    Restarting,
    /// The session replaced its own process image with a build that refused
    /// this client's protocol version. The koshi at the path this client was
    /// started from runs `koshi attach <session id>` in this terminal next.
    RestartedIntoIncompatibleBuild,
    /// The session replaced its own process image, and its new image answered
    /// this client's join with the refusal carried here. The session keeps
    /// running without this client.
    RefusedAfterRestart(Box<CliError>),
    /// A remote viewer's link broke and [`redial_remote_session`] gave up, carrying the cause it
    /// gave up on. The session keeps running without this viewer.
    LinkLost(Box<CliError>),
}

/// Two endings are equal when they are the same variant carrying the same
/// fields. A [`AttachmentEnding::SwitchSession`] compares its [`SessionId`]. A
/// [`AttachmentEnding::LinkLost`] and a [`AttachmentEnding::RefusedAfterRestart`]
/// compare the text their cause prints, which is what the viewer shows.
impl PartialEq for AttachmentEnding {
    fn eq(&self, other_ending: &Self) -> bool {
        match (self, other_ending) {
            (AttachmentEnding::Detached, AttachmentEnding::Detached)
            | (AttachmentEnding::SessionEnded, AttachmentEnding::SessionEnded)
            | (AttachmentEnding::ConnectionDied, AttachmentEnding::ConnectionDied)
            | (AttachmentEnding::TerminalGone, AttachmentEnding::TerminalGone)
            | (AttachmentEnding::Restarting, AttachmentEnding::Restarting)
            | (
                AttachmentEnding::RestartedIntoIncompatibleBuild,
                AttachmentEnding::RestartedIntoIncompatibleBuild,
            ) => true,
            (
                AttachmentEnding::SwitchSession(first_session_id),
                AttachmentEnding::SwitchSession(second_session_id),
            ) => first_session_id == second_session_id,
            (AttachmentEnding::LinkLost(first_cause), AttachmentEnding::LinkLost(second_cause)) => {
                first_cause.to_string() == second_cause.to_string()
            }
            (
                AttachmentEnding::RefusedAfterRestart(first_refusal),
                AttachmentEnding::RefusedAfterRestart(second_refusal),
            ) => first_refusal.to_string() == second_refusal.to_string(),
            _ => false,
        }
    }
}

/// One thing the loop reacts to: a frame read off the session's event stream,
/// or an event read from this terminal.
///
/// Both arrive on one channel, so one blocking read serves the session and the
/// keyboard at once.
enum Incoming {
    /// A frame the session wrote, or the read that failed.
    Frame {
        /// Which connection the reader that sent this was reading. A client
        /// that came back after the session replaced its own process image
        /// reads a subsequent connection: connection 0 is the first, 1 the one after
        /// the first restart, and so on.
        connection_index: u64,
        /// The frame itself, or the read that failed.
        session_event_result: Result<SessionEvent, IpcError>,
    },
    /// A key, a resize, or this terminal hanging up. Boxed to keep this type
    /// close to the size of a frame.
    Input(Box<RuntimeEvent>),
}

/// This client's side of the connection: the queue the writer thread takes
/// requests from, the action table a fired binding is resolved against, and the
/// number the next request carries.
struct Uplink {
    /// The queue [`spawn_uplink_writer`]'s thread writes from. Every request
    /// the loop sends goes here.
    request_sender: mpsc::Sender<IpcRequest>,
    /// The action table a fired binding is turned into commands with. The
    /// session owns its own table and runs those commands; this one only names
    /// what they are.
    registry: ActionRegistry,
    /// The `request_id` the next request carries.
    next_request_id: u64,
}

impl Uplink {
    /// Queue one request for the writer thread, numbering it, and give back the
    /// `request_id` it carried. The queue takes it whatever the socket is
    /// doing, so this never waits.
    ///
    /// A request over the frame cap is dropped by the writer and never
    /// answered. A queue nobody takes from is a writer thread that has ended,
    /// which only a broken connection does; the reading half meets that same
    /// connection and ends the loop.
    fn send_request(&mut self, request_kind: IpcRequestKind) -> u64 {
        let request_id = self.next_request_id;
        let ipc_request = IpcRequest {
            request_id,
            request_kind,
        };
        self.next_request_id += 1;
        let _ = self.request_sender.send(ipc_request);
        request_id
    }

    /// Queue one placement preview request when this client has no request in flight.
    pub fn send_placement_read(
        &mut self,
        client: &mut Client,
        source_pane_id: PaneId,
        destination_tab_id: TabId,
    ) -> Option<u64> {
        let request_id = self.next_request_id;
        if !client.begin_placement_read(
            request_id,
            source_pane_id,
            destination_tab_id,
            Instant::now(),
        ) {
            return None;
        }
        Some(self.send_request(IpcRequestKind::ReadPanePlacement {
            pane_id: source_pane_id,
            destination_tab_id,
        }))
    }

    /// Queue placement command `command`, which `client` recorded as pending
    /// under `command_id`.
    fn submit_placement_command(
        &mut self,
        client: &Client,
        command_id: CommandId,
        command: Command,
    ) {
        let command_envelope = CommandEnvelope::from_parts(
            command_id,
            CommandSource::from_key_binding(client.get_client_id()),
            command,
        );
        self.send_request(IpcRequestKind::SubmitCommand(Box::new(command_envelope)));
    }

    /// Queue focus for the pane picked up by a mouse drag. Picking up `pane-123`
    /// sends focus to `pane-123`.
    fn submit_mouse_focus_pane(&mut self, client: &Client, pane_id: PaneId) {
        let command_envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_mouse(client.get_client_id()),
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(pane_id),
                client_id: Some(client.get_client_id()),
            }),
        );
        self.send_request(IpcRequestKind::SubmitCommand(Box::new(command_envelope)));
    }

    /// Send what one placement key or mouse event asks of the session:
    ///
    /// - `ReadPlacement` sends one preview read, then `FocusPane` for
    ///   `pane_id_to_focus` when it is `Some`.
    /// - `SubmitPlacement` sends the placement command under its `command_id`.
    /// - `Consumed` sends nothing.
    fn submit_placement_input_action(
        &mut self,
        client: &mut Client,
        placement_input_action: PlacementInputAction,
    ) {
        match placement_input_action {
            PlacementInputAction::ReadPlacement {
                pane_id_to_focus,
                source_pane_id,
                destination_tab_id,
            } => {
                self.send_placement_read(client, source_pane_id, destination_tab_id);
                if let Some(pane_id_to_focus) = pane_id_to_focus {
                    self.submit_mouse_focus_pane(client, pane_id_to_focus);
                }
            }
            PlacementInputAction::SubmitPlacement {
                command_id,
                command,
            } => {
                self.submit_placement_command(client, command_id, command);
            }
            PlacementInputAction::Consumed => {}
        }
    }

    /// Resolve one fired binding against the action table and run its plan.
    ///
    /// `new_pane_direction` is this viewer's own setting, so a pane-opening
    /// binding that names no direction already says where the pane goes by the
    /// time the command leaves. A viewer-local action runs here; an action the
    /// table refuses sends no session command.
    fn submit_bound_action(&mut self, client: &mut Client, bound_action: BoundAction) {
        let new_pane_direction = client.get_client_config().layout.new_pane_direction;
        let Ok(dispatch_plan) = resolve_action_with_scroll_line_count(
            &bound_action.action_reference,
            &self.registry,
            new_pane_direction,
            client.get_client_config().mouse.scroll_line_count,
        ) else {
            return;
        };
        match dispatch_plan {
            DispatchPlan::Command(command) => {
                let command_envelope = CommandEnvelope::from_parts(
                    CommandId::new(),
                    CommandSource::from_key_binding(client.get_client_id()),
                    *command,
                );
                self.send_request(IpcRequestKind::SubmitCommand(Box::new(command_envelope)));
            }
            DispatchPlan::ClientAction(client_action_kind) => {
                let placement_input_action = client.apply_client_action(client_action_kind);
                self.submit_placement_input_action(client, placement_input_action);
            }
        }
    }
}

/// Where the session this client joins runs.
///
/// One home is picked before the first connection opens and holds for every
/// connection after it, so a switch and a restart re-enter the session the same
/// way the first join entered it.
enum Home {
    /// A session on this machine, joined through the endpoint file it
    /// advertises under `runtime_directory`.
    Local {
        /// This user's runtime directory, holding one endpoint file per
        /// session.
        runtime_directory: PathBuf,
    },
    /// A session on another machine, joined through the server serving it.
    Remote {
        /// The saved record every dial presents: the address, the secret, and
        /// the certificate fingerprint pinned on the first connection.
        server: ServerReference,
    },
}

/// One open connection into a session, past the join.
struct JoinedSession {
    /// The frames the session sends.
    reader: FrameReader,
    /// The frames this client sends.
    writer: FrameWriter,
    /// The client the server minted for this terminal.
    client_id: ClientId,
    /// The session the server says that client joined.
    session_id: SessionId,
    /// The token this connection was opened under: on this machine the
    /// session's endpoint token, which changes every time that session binds a
    /// new socket, and on a server the secret that server admitted.
    connection_token: ConnectionToken,
    /// The secret this attach minted, presented on the next attach to get this
    /// attach's view back. `None` from a session server that mints none.
    resume_token: Option<ConnectionToken>,
}

/// Resolve what the user typed to one running session, and report where it
/// listens.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. `None` picks one from the sessions running for this user and the
/// sessions `shared_sessions_base_directory` advertises instead: nothing
/// running is a failure, one session is taken straight away, and more than one
/// is printed as a numbered list to answer on stdin.
fn resolve_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    selector: Option<&str>,
) -> Result<SessionAddress, CliError> {
    let selector = match selector {
        Some(selector) => selector.to_string(),
        // No remote rows are offered, so every place is a local one.
        None => match select_session(
            runtime_directory,
            shared_sessions_base_directory,
            Vec::new(),
        )? {
            SessionSelection::Local(session_id) => session_id,
            SessionSelection::Remote(remote_row_index) => {
                unreachable!(
                    "a listing offered no remote rows and settled on place {remote_row_index}"
                )
            }
        },
    };
    lookup_session_address(runtime_directory, &selector)
}

/// Join a running session in this terminal as a new client.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. `None` picks one from the sessions running for this user and the
/// sessions on every saved server that answered: no session running anywhere
/// is a failure, exactly one session on this machine is taken straight away,
/// and anything else — several sessions, or one session on a saved server —
/// is printed as a numbered list to answer on stdin.
pub fn attach_selected_session(selector: Option<&str>) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let config_directory = koshi_paths::resolve_config_directory();
    let Some(selector) = selector else {
        return attach_selected_session_from_listing(
            runtime_directory,
            config_directory.as_deref(),
        );
    };
    let session_address = lookup_session_address(&runtime_directory, selector)?;
    attach_home(
        &Home::Local { runtime_directory },
        SessionSelector::SessionId(session_address.session_id),
        config_directory.as_deref(),
    )
}

/// Join a session on the machine `server` names in this terminal.
///
/// `server` is either the name this machine saved that server under or the
/// `host:port` it listens on. `save_as` is the name to save a server reached
/// for the first time under; a server already saved refuses it.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display name,
/// and the server resolves it against the sessions this machine's secret
/// reaches. `None` lists those sessions instead: one is taken straight away,
/// and more than one is printed as a numbered list to answer on stdin. Nothing
/// here creates a session.
///
/// # Errors
/// [`CliError::Runtime`] when the server does not admit the secret, when the
/// certificate it presents is not the pinned one, and when nothing this secret
/// reaches is running on it.
pub fn attach_remote_session(
    server: &str,
    save_as: Option<&str>,
    selector: Option<&str>,
) -> Result<(), CliError> {
    let server_reference = remote_client::resolve_server(server)?;
    // This connection carries one listing. The attachment below dials its own.
    let (mut server_connection, saved_server) = remote_client::connect_saved_server(
        &server_reference,
        save_as,
        Some(remote_client::REPLY_TIMEOUT_DURATION),
    )?;
    let session_selector = match selector {
        Some(selector) => build_session_selector(selector),
        None => select_remote_session(
            server,
            &remote_client::list_remote_sessions(&mut server_connection)?,
        )?,
    };
    // The attachment dials its own connection, so this one is finished with.
    drop(server_connection);
    attach_home(
        &Home::Remote {
            server: ServerReference::Saved(saved_server),
        },
        session_selector,
        koshi_paths::resolve_config_directory().as_deref(),
    )
}

/// Join the session a bare `koshi attach` picks, from this user's own sessions
/// and the sessions on every saved server that answered.
///
/// The saved servers are asked through
/// [`list_saved_server_session_rows`](remote_client::list_saved_server_session_rows),
/// which prints one stderr line for each server whose sessions it does not
/// list, such as a server that refused or did not answer.
///
/// `config_directory` holds the `koshi.kdl` that names the shared directory
/// other users' sessions are listed from, and the config the attachment reads.
fn attach_selected_session_from_listing(
    runtime_directory: PathBuf,
    config_directory: Option<&Path>,
) -> Result<(), CliError> {
    let reachable_session_rows = remote_client::list_saved_server_session_rows("attach");
    let remote_session_rows = reachable_session_rows
        .iter()
        .map(|(server_label, remote_session_row)| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some(server_label.clone()),
            )
        })
        .collect();
    let shared_sessions_base_directory =
        koshi_link::config::find_shared_sessions_base_directory(config_directory);
    let (server_label, selected_session_row) = match select_session(
        &runtime_directory,
        shared_sessions_base_directory.as_deref(),
        remote_session_rows,
    )? {
        SessionSelection::Local(session_identifier) => {
            let session_address = lookup_session_address(&runtime_directory, &session_identifier)?;
            return attach_home(
                &Home::Local { runtime_directory },
                SessionSelector::SessionId(session_address.session_id),
                config_directory,
            );
        }
        SessionSelection::Remote(remote_session_index) => {
            &reachable_session_rows[remote_session_index]
        }
    };
    attach_home(
        &Home::Remote {
            server: remote_client::resolve_server(server_label)?,
        },
        SessionSelector::SessionId(selected_session_row.session_id),
        config_directory,
    )
}

/// The session a `koshi attach --remote <server>` with no session named joins,
/// picked from the sessions that server's secret reaches.
///
/// One row is the answer on its own; more than one is printed and the number
/// typed on stdin picks the row. This runs before the terminal enters raw mode,
/// so the prompt is a plain stdin read.
///
/// # Errors
/// [`CliError::Runtime`] when the secret reaches no running session on that
/// server.
fn select_remote_session(
    server_label: &str,
    remote_session_rows: &[RemoteSessionRow],
) -> Result<SessionSelector, CliError> {
    if remote_session_rows.is_empty() {
        return Err(CliError::Runtime {
            detail: format!("no session is reachable on {server_label}"),
        });
    }
    let listed_session_rows: Vec<SessionRow> = remote_session_rows
        .iter()
        .map(|remote_session_row| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some(server_label.to_string()),
            )
        })
        .collect();
    let selected_session_row_index = select_session_index(&listed_session_rows)?;
    Ok(SessionSelector::SessionId(
        listed_session_rows[selected_session_row_index].session_id,
    ))
}

/// Ask the session this CLI runs inside to move its own client to another
/// session.
///
/// `selector` names the session to move to: a `session-<uuid>` id, a bare
/// UUID, or a session display name, and `None` picks one from the sessions
/// running for this user. A session on another machine is never offered, since
/// this session cannot move a client into one. The session moves the client
/// this terminal already holds.
pub fn switch_in_session(
    session_context: &InSessionContext,
    selector: Option<&str>,
) -> Result<CommandResult, CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let shared_sessions_base_directory = ipc_client::resolve_shared_sessions_base_directory();
    let address = resolve_session(
        &runtime_directory,
        shared_sessions_base_directory.as_deref(),
        selector,
    )?;
    ipc_client::submit_in_session_command(
        session_context,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: address.session_id,
        }),
    )
}

/// Join the session `session_id` names and run until this client detaches, the
/// session ends, the connection breaks, or the session moves this client to
/// another session, in which case this attaches there and keeps running.
///
/// Paints every frame the session composes for this terminal and sends this
/// terminal's keys, mouse and resizes back. A broken connection reports the
/// cause and how to reattach, and exits non-zero; the other endings print what
/// happened and exit zero.
///
/// `config_directory` holds the `koshi.kdl` that names the shared directory
/// searched when `runtime_directory` advertises no `session_id`, and the
/// config the attachment reads. `None` searches `runtime_directory` alone and
/// attaches with the built-in config.
pub(crate) fn attach_session(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<(), CliError> {
    ipc_client::load_session_endpoint(
        runtime_directory,
        koshi_link::config::find_shared_sessions_base_directory(config_directory).as_deref(),
        session_id,
    )?;
    attach_home(
        &Home::Local {
            runtime_directory: runtime_directory.to_path_buf(),
        },
        SessionSelector::SessionId(session_id),
        config_directory,
    )
}

/// Join the session `session_selector` names in `home` and run until nothing moves this
/// client on.
///
/// A session that moves this client to another one is attached to next, in the
/// same home: a client on a server dials that server again for it, so the
/// certificate, the secret and the scope are all checked again before the next
/// session paints anything.
///
/// The path this program was started from, as
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// reads it, is read once, here, before the first attachment. `None` when the
/// platform cannot report it. `config_directory` is handed to every
/// attachment.
fn attach_home(
    home: &Home,
    session_selector: SessionSelector,
    config_directory: Option<&Path>,
) -> Result<(), CliError> {
    let client_executable_path = koshi_host::program_path::resolve_program_path().ok();
    let mut next_session_selector = session_selector;
    while let Some(next_session_id) = attach_once(
        home,
        &next_session_selector,
        client_executable_path.as_deref(),
        config_directory,
    )? {
        next_session_selector = SessionSelector::SessionId(next_session_id);
    }
    Ok(())
}

/// Join the session `session_selector` names in `home` and run one attachment of it,
/// handing back the session to attach to next when this one moved the client
/// on.
///
/// The terminal enters raw mode for its capability probe, then enters the
/// alternate screen after Attach succeeds. This call owns both states and
/// leaves them before it returns, so the terminal is restored between one
/// session and the next.
///
/// A session that replaces its own process image is handled inside this one
/// attachment: the client comes back as the same client — on the session's new
/// socket on this machine, and through a fresh dial of the server otherwise —
/// and the terminal keeps every mode it is in, so nothing on the screen
/// flickers.
///
/// `client_executable_path` is the path this program was started from, which
/// [`report_attachment_ending`] runs `koshi attach` from when the session
/// restarted into a build that refused this client's protocol version.
///
/// `config_directory` holds the config files read at the start of each
/// attachment, and the `koshi.kdl` each dial on this machine reads for the
/// shared directory.
fn attach_once(
    home: &Home,
    session_selector: &SessionSelector,
    client_executable_path: Option<&Path>,
    config_directory: Option<&Path>,
) -> Result<Option<SessionId>, CliError> {
    let (loaded_config, config_warnings) = koshi_link::config::load_config_files(config_directory);
    let supports_native_images =
        koshi_link::config::supports_image_output(loaded_config.app_config_layer.clone());
    let mut terminal_owner = terminal::TerminalOwner::open_terminal_owner(supports_native_images)
        .map_err(|detail| CliError::Runtime { detail })?;
    let graphics_support = terminal_owner.get_graphics_support();
    let mut cell_size_query = terminal_owner.build_cell_size_query();
    let JoinedSession {
        reader,
        writer,
        client_id,
        session_id,
        connection_token,
        resume_token,
    } = dial_session(
        home,
        session_selector,
        graphics_support,
        cell_size_query.get_current_cell_size(),
        config_directory,
    )?;

    // After the session accepts the client, the terminal changes mode. The
    // cleanup hooks undo every mode this function sets. The panic hook runs the
    // same hooks: an unwinding panic restores the terminal and then writes a
    // crash report into the data directory.
    let terminal_cleanup_guard = TerminalCleanupGuard::new();
    terminal_owner.register_terminal_restore(&terminal_cleanup_guard);
    let _panic_guard = install_panic_hook(
        &terminal_cleanup_guard,
        koshi_paths::resolve_data_directory(),
    );

    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let (input_sender, input_receiver) = build_input_channel();
    let should_read_input = io::stdin().is_tty();
    terminal_owner
        .activate_terminal(input_sender, client_id, should_read_input)
        .map_err(|activation_detail| CliError::Runtime {
            detail: activation_detail,
        })?;
    if should_read_input {
        spawn_input_relay(input_receiver, incoming_sender.clone());
    } else {
        drop(input_receiver);
        tracing::info!("standard input is not a terminal, so this client reads no keys");
    }
    // The ratatui terminal owns the output side; the renderer paints its
    // buffer. A terminal that reports no size — which is what a `koshi
    // attach` with redirected output finds — gets a buffer of
    // [`FALLBACK_VIEWPORT`], the size this client told the session it has.
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout())).unwrap_or_else(
        |terminal_creation_error| {
            tracing::warn!(%terminal_creation_error, "could not size the output terminal");
            Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(
                        0,
                        0,
                        FALLBACK_VIEWPORT.column_count,
                        FALLBACK_VIEWPORT.row_count,
                    )),
                },
            )
            .expect("a fixed viewport reads no terminal size")
        },
    );

    // One channel, three producers: the reading half of every connection this
    // attachment holds in turn, this terminal's input thread, and the loop
    // itself, which keeps a sender to start the reader it comes back on. A
    // broken connection reaches the loop as the failed read its own reader
    // writes here, so the loop ends on that frame, not on the channel closing.
    spawn_frame_reader(reader, INITIAL_CONNECTION_INDEX, incoming_sender.clone());

    // The viewer half: this terminal's own keymap, colors and hint bar, read
    // from this user's config files. Its frames arrive over the connection,
    // and the delivery receiver it holds has no sender. It holds the cleanup
    // guard that restores this outer terminal.
    // The subscriber this client writes its own log through. `koshi attach`
    // installs none before this point; a bare `koshi` already has one, and
    // this call answers `AlreadyInitialized` for it.
    let _ = koshi_observability::logging::initialize_tracing(
        koshi_link::config::build_logging_parameters(
            loaded_config.app_config_layer.as_ref(),
            session_id,
        ),
    );
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }
    let (_delivery_sender, delivery_receiver) = mpsc::channel();
    let mut client = terminal::build_client_with_loaded_config(
        client_id,
        get_terminal_viewport_size(),
        delivery_receiver,
        terminal_cleanup_guard,
        loaded_config,
    );
    client.set_session_id(session_id);
    let mut uplink = Uplink {
        request_sender: spawn_uplink_writer(writer),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
    };
    let mut screen = Screen::with_graphics_support(
        terminal,
        client.get_viewport_size(),
        graphics_support,
        cell_size_query.get_current_cell_size(),
    );

    let mut terminal_mode_writer = io::stdout();
    let ending = run_attachment(
        home,
        session_id,
        client_id,
        connection_token,
        resume_token,
        &mut client,
        &mut screen,
        &mut uplink,
        graphics_support,
        &mut cell_size_query,
        &mut terminal_mode_writer,
        incoming_sender,
        incoming_receiver,
    );

    // The terminal is restored before the ending message is printed, and the
    // message lands on the shell's own screen. Dropping the screen drops the
    // ratatui terminal it holds, which shows the cursor on the alternate
    // screen. Dropping the client then runs its cleanup guard, which leaves
    // the alternate screen.
    drop(screen);
    terminal_owner.shutdown_terminal();
    drop(client);
    report_attachment_ending(home, ending, session_id, client_executable_path)
}

/// Run one attachment: paint every frame the session sends, send this
/// terminal's keys, mouse and resizes back, and report how the stream ended.
///
/// One loop serves both homes. Everything transport-shaped is already settled
/// by the time it starts: the connection arrives as the two halves behind
/// `uplink` and `incoming_receiver`, and a session replacing its own process image
/// comes back through [`reconnect_after_restart`], which re-enters `home` the way that home
/// is entered.
///
/// A remote viewer whose link breaks comes back through [`redial_remote_session`], which is
/// handed this loop's `client` and `screen`: it paints the
/// `RECONNECTING (attempt 1, retry in 1s)` tag once a second, dials the server
/// again on a widening pause, and drops everything typed while it had no link. A
/// dial it gives up on ends the loop as [`AttachmentEnding::LinkLost`] carrying the cause.
/// A local viewer whose link breaks ends the loop, as it always has.
///
/// `connection_token` is the token the open connection was opened under, which
/// [`reconnect_after_restart`] reads and stamps with the token of the connection this client
/// comes back on.
///
/// `client_id` is the client the open connection joined as. A redial or a
/// return after a restart that a session answers with a fresh client replaces
/// it, and the viewer's own id with it.
///
/// `resume_token` is the secret the open connection's attach minted, presented
/// on the next redial to get that attach's view back. Every reconnection
/// stamps it with the secret its own attach minted.
#[allow(clippy::too_many_arguments)]
fn run_attachment<B: Backend, W: Write>(
    home: &Home,
    session_id: SessionId,
    mut client_id: ClientId,
    mut connection_token: ConnectionToken,
    mut resume_token: Option<ConnectionToken>,
    client: &mut Client,
    screen: &mut Screen<B>,
    uplink: &mut Uplink,
    graphics_support: terminal::GraphicsSupport,
    cell_size_query: &mut terminal::CellSizeQuery,
    terminal_mode_writer: &mut W,
    incoming_sender: mpsc::SyncSender<Incoming>,
    incoming_receiver: mpsc::Receiver<Incoming>,
) -> AttachmentEnding {
    // Which connection the loop is reading. Coming back after the session
    // replaces its own process image counts up, and the loop drops every frame
    // that does not carry this number.
    let mut current_connection_index: u64 = INITIAL_CONNECTION_INDEX;
    let mut last_mouse_frame: Option<MouseFrame> = None;
    let mut image_cache = paint::ImageCache::new();
    let mut deferred_incoming_event = None;
    // What the viewer has decided and not yet written. The pass that decided it
    // ends by writing all of it, so this holds one pass's worth: the events that
    // arrived together. It is bounded by [`MAX_PENDING_MOUSE_ACTION_COUNT`], which every path
    // that adds to it goes through [`queue_mouse_actions`] to keep.
    let mut pending_mouse_actions: Vec<MouseAction> = Vec::new();
    // The border moves written and not yet answered, newest last. A move names
    // the whole distance from the drag anchor and the anchor advances only on an
    // answer, so these are the cells the next move for the same border must not
    // ask for a second time.
    let mut sent_border_moves: Vec<SentBorderMove> = Vec::new();

    loop {
        let current_time = Instant::now();
        let next_wakeup_duration = select_earliest_duration(
            select_earliest_duration(
                client.compute_next_key_wakeup(current_time),
                client.compute_next_mouse_wakeup(current_time),
            ),
            select_earliest_duration(
                client.compute_next_placement_read_wakeup(current_time),
                select_earliest_duration(
                    client.compute_next_placement_tab_hover_wakeup(current_time),
                    select_earliest_duration(
                        screen.compute_next_image_wakeup_at(current_time),
                        screen.compute_next_placement_animation_wakeup_at(current_time),
                    ),
                ),
            ),
        );
        let incoming_event = match deferred_incoming_event.take() {
            Some(incoming_event) => Some(incoming_event),
            None => match next_wakeup_duration {
                Some(wakeup_duration) => match incoming_receiver.recv_timeout(wakeup_duration) {
                    Ok(incoming_event) => Some(incoming_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break AttachmentEnding::ConnectionDied;
                    }
                },
                None => match incoming_receiver.recv() {
                    Ok(incoming_event) => Some(incoming_event),
                    Err(_) => break AttachmentEnding::ConnectionDied,
                },
            },
        };
        // Take a bounded group of queued events. A full group leaves the next
        // event in the channel, so the next pass starts without waiting and
        // image output, key timeouts, and outbound requests run between groups.
        let (incoming_batch_events, deferred_event) =
            build_incoming_batch(incoming_event, &incoming_receiver);
        deferred_incoming_event = deferred_event;

        let mut attachment_ending = None;
        for incoming_event in incoming_batch_events {
            match incoming_event {
                // A frame read from a connection this client has already left:
                // the reader of the connection before a restart ends by
                // reporting that socket closing.
                Incoming::Frame {
                    connection_index, ..
                } if connection_index != current_connection_index => {}
                Incoming::Frame {
                    session_event_result,
                    ..
                } => {
                    if let Some(frame_ending) = classify_session_event(&session_event_result) {
                        attachment_ending = Some(frame_ending);
                        break;
                    }
                    match session_event_result {
                        Ok(SessionEvent::Painted {
                            frame: painted_frame,
                        }) => match image_cache.adopt_painted_frame(painted_frame) {
                            Ok(Some(render_snapshot)) => {
                                screen.set_cell_size(cell_size_query.get_current_cell_size());
                                if let Some(mouse_frame) =
                                    screen.draw_snapshot(client, render_snapshot)
                                {
                                    last_mouse_frame = Some(mouse_frame);
                                }
                                if client.get_placement_snapshot().is_none() {
                                    if let Err(snapshot_error) =
                                        image_cache.clear_placement_snapshot()
                                    {
                                        tracing::warn!(
                                            %snapshot_error,
                                            "could not clear a stale placement snapshot"
                                        );
                                        attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                        break;
                                    }
                                    screen.set_placement_snapshot(None);
                                }
                            }
                            Ok(None) => {}
                            Err(painted_frame_error) => {
                                tracing::warn!(
                                    %painted_frame_error,
                                    "could not accept a painted frame"
                                );
                                attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                break;
                            }
                        },
                        Ok(SessionEvent::ImageCacheReset) => {
                            image_cache.clear_image_cache();
                            client.clear_placement_snapshot();
                            screen.set_placement_snapshot(None);
                            screen.reset_connection();
                        }
                        Ok(SessionEvent::ImageContentStart { image_transfer }) => {
                            if let Err(transfer_error) =
                                image_cache.start_image_transfer(image_transfer)
                            {
                                tracing::warn!(%transfer_error, "could not start an image transfer");
                                attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                break;
                            }
                        }
                        Ok(SessionEvent::ImageContentChunk { image_chunk }) => {
                            match image_cache.accept_image_chunk(image_chunk) {
                                Ok(Some(render_snapshot)) => {
                                    screen.set_placement_snapshot(
                                        image_cache.build_placement_render_snapshot(),
                                    );
                                    screen.set_cell_size(cell_size_query.get_current_cell_size());
                                    if let Some(mouse_frame) =
                                        screen.draw_snapshot(client, render_snapshot)
                                    {
                                        last_mouse_frame = Some(mouse_frame);
                                    }
                                    if client.get_placement_snapshot().is_none() {
                                        if let Err(snapshot_error) =
                                            image_cache.clear_placement_snapshot()
                                        {
                                            tracing::warn!(
                                                %snapshot_error,
                                                "could not clear a stale placement snapshot"
                                            );
                                            attachment_ending =
                                                Some(AttachmentEnding::ConnectionDied);
                                            break;
                                        }
                                        screen.set_placement_snapshot(None);
                                    }
                                }
                                Ok(None) => {
                                    screen.set_placement_snapshot(
                                        image_cache.build_placement_render_snapshot(),
                                    );
                                }
                                Err(chunk_error) => {
                                    tracing::warn!(%chunk_error, "could not accept an image chunk");
                                    attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                    break;
                                }
                            }
                        }
                        Ok(SessionEvent::PanePlacementSnapshot {
                            request_id,
                            snapshot,
                        }) => {
                            let placement_snapshot = *snapshot;
                            if !client.accept_placement_snapshot(
                                request_id,
                                placement_snapshot.clone(),
                                Instant::now(),
                            ) {
                                tracing::debug!(request_id, "ignored stale placement snapshot");
                                image_cache.ignore_stale_placement_image_transfers();
                                send_next_queued_placement_read(client, uplink);
                                continue;
                            }
                            if let Err(snapshot_error) =
                                image_cache.adopt_placement_snapshot(Box::new(placement_snapshot))
                            {
                                tracing::warn!(%snapshot_error, "could not accept a placement snapshot");
                                attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                break;
                            }
                            screen.set_placement_snapshot(
                                image_cache.build_placement_render_snapshot(),
                            );
                            send_next_queued_placement_read(client, uplink);
                        }
                        Ok(SessionEvent::PanePlacementRefused {
                            request_id,
                            refusal,
                        }) => {
                            let is_refusal_accepted = client.accept_placement_refusal(
                                request_id,
                                refusal.code,
                                Instant::now(),
                            );
                            if !is_refusal_accepted {
                                tracing::debug!(request_id, "ignored stale placement refusal");
                            }
                            if is_refusal_accepted || !client.is_placement_read_pending() {
                                send_next_queued_placement_read(client, uplink);
                            }
                            tracing::debug!(request_id, message = %refusal.message, "placement preview refused");
                        }
                        Ok(SessionEvent::PanePlacementCommitted {
                            command_id,
                            source_tab_id,
                            destination_tab_id,
                            ..
                        }) => {
                            if !client.note_placement_command_committed(command_id) {
                                screen
                                    .note_committed_placement([source_tab_id, destination_tab_id]);
                            }
                        }
                        Ok(SessionEvent::PlacementCommandRejected { command_id }) => {
                            if !client.reject_placement_command(command_id) {
                                tracing::debug!(%command_id, "ignored stale placement command rejection");
                            }
                        }
                        Ok(SessionEvent::MouseAnswer {
                            request_id,
                            mouse_answers,
                        }) => {
                            if let Some(mouse_frame) = last_mouse_frame.as_ref() {
                                apply_mouse_answers(
                                    client,
                                    mouse_frame,
                                    &mut sent_border_moves,
                                    request_id,
                                    mouse_answers,
                                    &mut pending_mouse_actions,
                                );
                            }
                        }
                        // The stream dropped events, and the answers to the
                        // border moves now on the wire may be among them. A
                        // resync forgets every remembered move, and the next
                        // move asks for its whole distance from the drag
                        // anchor. Nothing is released by this: no round ever
                        // waited.
                        Ok(SessionEvent::Resync { .. }) => {
                            sent_border_moves.clear();
                            client.prepare_placement_reconciliation();
                        }
                        // Bytes a pane aimed at this terminal, such as an OSC 52
                        // clipboard write, go to it verbatim.
                        Ok(SessionEvent::HostWrite { host_output_bytes }) => {
                            let mut stdout = io::stdout();
                            let _ = stdout.write_all(&host_output_bytes);
                            let _ = stdout.flush();
                        }
                        // Every other frame reports a change to the session's
                        // structure, and the next painted frame carries that
                        // change whole, so only a painted frame is drawn.
                        _ => {}
                    }
                }
                // An input thread runs only for a terminal that had keys to
                // read, so its hangup is this terminal going away while the
                // session runs on.
                Incoming::Input(runtime_event) if matches!(*runtime_event, RuntimeEvent::Quit) => {
                    attachment_ending = Some(AttachmentEnding::TerminalGone);
                    break;
                }
                Incoming::Input(runtime_event) => match *runtime_event {
                    // A mouse event belongs to the viewer: the frame it painted
                    // says which pane the pointer is over, what that pane's
                    // program asked for, and which gesture is under way. Before
                    // the first paint there is no frame to place it against.
                    RuntimeEvent::MouseInput { mouse_input, .. } => {
                        if let Some(mouse_frame) = last_mouse_frame.as_ref() {
                            client.apply_events();
                            if !handle_placement_mouse_event(
                                client,
                                uplink,
                                mouse_frame,
                                mouse_input,
                            ) {
                                handle_mouse_event(
                                    client,
                                    mouse_frame,
                                    mouse_input,
                                    &mut pending_mouse_actions,
                                );
                            }
                        }
                    }
                    RuntimeEvent::OuterTerminalFocusLost { .. } => {
                        client.cancel_placement_mode();
                    }
                    other_runtime_event => {
                        process_runtime_input_with_cell_size(
                            client,
                            uplink,
                            cell_size_query,
                            other_runtime_event,
                        );
                    }
                },
            }
        }
        if let Some(attachment_ending) = attachment_ending {
            let reconnection = match attachment_ending {
                AttachmentEnding::Restarting => {
                    // The session is replacing its own process image. The
                    // terminal keeps every mode it is in and the screen is left
                    // alone; the session's first frame on the new connection
                    // paints the panes again.
                    //
                    // The last request on this connection. The queue is written
                    // in order, so it leaves behind every key already on it.
                    uplink.send_request(IpcRequestKind::Leaving);
                    client.prepare_placement_reconciliation();
                    screen.set_placement_snapshot(None);
                    match reconnect_after_restart(
                        home,
                        session_id,
                        client_id,
                        &mut connection_token,
                        &mut resume_token,
                        graphics_support,
                        cell_size_query.get_current_cell_size(),
                    ) {
                        Ok((rejoined_client_id, reader, writer)) => {
                            if rejoined_client_id != client_id {
                                tracing::warn!(
                                    %session_id,
                                    "the restarted session did not hand this client back; this viewer goes on as the new client it minted"
                                );
                            }
                            Some((rejoined_client_id, reader, writer))
                        }
                        Err(restart_ending) => break restart_ending,
                    }
                }
                // The link broke. A viewer of a session on a server, with
                // `remote-reconnect` on, dials that server again while the
                // tabline reads `RECONNECTING (attempt 1, retry in 1s)`;
                // nothing typed over that stretch is sent. A dial this client
                // gave up on ends as [`AttachmentEnding::LinkLost`] carrying its cause. A
                // viewer with `remote-reconnect` off, and a viewer of a session
                // on this machine, end here.
                AttachmentEnding::ConnectionDied => match home {
                    Home::Remote { server }
                        if client.get_client_config().should_reconnect_remote_session =>
                    {
                        client.prepare_placement_reconciliation();
                        screen.set_placement_snapshot(None);
                        match redial_remote_session(
                            server,
                            session_id,
                            resume_token.as_ref(),
                            client,
                            screen,
                            last_mouse_frame
                                .as_ref()
                                .map(|mouse_frame| mouse_frame.client_snapshot.active_tab_id),
                            graphics_support,
                            cell_size_query.get_current_cell_size(),
                        ) {
                            Ok(joined_session) => {
                                resume_token = joined_session.resume_token;
                                if drop_input_from_the_blackout(&incoming_receiver, cell_size_query)
                                {
                                    break AttachmentEnding::TerminalGone;
                                }
                                Some((
                                    joined_session.client_id,
                                    joined_session.reader,
                                    joined_session.writer,
                                ))
                            }
                            Err(cause) => break AttachmentEnding::LinkLost(cause),
                        }
                    }
                    Home::Remote { .. } | Home::Local { .. } => None,
                },
                AttachmentEnding::Detached
                | AttachmentEnding::SessionEnded
                | AttachmentEnding::TerminalGone
                | AttachmentEnding::SwitchSession(_)
                | AttachmentEnding::RestartedIntoIncompatibleBuild
                | AttachmentEnding::RefusedAfterRestart(_)
                | AttachmentEnding::LinkLost(_) => None,
            };
            let Some((rejoined_client_id, reader, writer)) = reconnection else {
                break attachment_ending;
            };
            client_id = rejoined_client_id;
            client.set_client_id(rejoined_client_id);
            client.end_mouse_gestures();
            pending_mouse_actions.clear();
            // The panes the old frame placed may be gone. Mouse events wait for
            // the new connection's first frame to be placed against.
            last_mouse_frame = None;
            current_connection_index += 1;
            image_cache.clear_image_cache();
            spawn_frame_reader(reader, current_connection_index, incoming_sender.clone());
            // Dropping the queue the old connection's writer thread reads from
            // is what ends that thread.
            uplink.request_sender = spawn_uplink_writer(writer);
            uplink.next_request_id = FIRST_POST_ATTACH_REQUEST_ID;
            let locally_measured_cell_size = terminal::read_local_cell_size();
            report_terminal_size_with_cell_size(
                client,
                uplink,
                cell_size_query,
                locally_measured_cell_size,
            );
            // The new connection numbers its rounds from the start, so no
            // answer to a border move written on the old one can arrive. The
            // next move asks for its whole distance from the drag anchor.
            sent_border_moves.clear();
            screen.reset_connection();
            if let Err(io_error) = screen.update_shift_mouse_capture(client, terminal_mode_writer) {
                tracing::warn!(%io_error, "could not update Shift mouse reporting");
                break AttachmentEnding::TerminalGone;
            }
            continue;
        }
        let current_time = Instant::now();
        fire_expired_key_sequence(client, uplink, current_time);
        if client.expire_placement_read(current_time) {
            send_next_queued_placement_read(client, uplink);
        }
        if let Some((source_pane_id, destination_tab_id)) =
            client.expire_placement_tab_hover(current_time)
        {
            uplink.send_placement_read(client, source_pane_id, destination_tab_id);
        }
        if let Some((source_pane_id, destination_tab_id)) = client.take_placement_preview_refresh()
        {
            uplink.send_placement_read(client, source_pane_id, destination_tab_id);
        }
        // A selection drag held past a pane's edge keeps scrolling while the
        // pointer sits still, so the clock drives it. Asking on every iteration
        // is what re-arms the timer at each firing.
        if let Some(mouse_frame) = last_mouse_frame.as_ref() {
            queue_mouse_actions(
                &mut pending_mouse_actions,
                client.expire_mouse_scroll(Instant::now(), mouse_frame),
            );
        }
        // Every pass ends here, whether or not it drew a frame: the events it
        // handled may have moved the viewer after that frame was drawn.
        screen.set_cell_size(cell_size_query.get_current_cell_size());
        if let Some(mouse_frame) = screen.refresh_at(
            client,
            last_mouse_frame
                .as_ref()
                .map(|mouse_frame| mouse_frame.client_snapshot.active_tab_id),
            Instant::now(),
        ) {
            last_mouse_frame = Some(mouse_frame);
        }
        if let Err(io_error) = screen.update_shift_mouse_capture(client, terminal_mode_writer) {
            tracing::warn!(%io_error, "could not update Shift mouse reporting");
            break AttachmentEnding::TerminalGone;
        }
        flush_mouse_round(uplink, &mut sent_border_moves, &mut pending_mouse_actions);
    }
}

/// Add `first_incoming_event` to the attachment-loop batch up to its limit.
fn build_incoming_batch(
    first_incoming_event: Option<Incoming>,
    incoming_receiver: &mpsc::Receiver<Incoming>,
) -> (Vec<Incoming>, Option<Incoming>) {
    let mut incoming_batch_events: Vec<Incoming> = first_incoming_event.into_iter().collect();
    let mut incoming_image_byte_count = incoming_batch_events
        .iter()
        .map(compute_incoming_image_byte_count)
        .sum::<usize>();
    while incoming_batch_events.len() < MAX_INCOMING_EVENT_COUNT_PER_PASS {
        let Ok(next_incoming_event) = incoming_receiver.try_recv() else {
            break;
        };
        let next_image_byte_count = compute_incoming_image_byte_count(&next_incoming_event);
        if !incoming_batch_events.is_empty()
            && incoming_image_byte_count.saturating_add(next_image_byte_count)
                > MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH
        {
            return (incoming_batch_events, Some(next_incoming_event));
        }
        incoming_image_byte_count = incoming_image_byte_count.saturating_add(next_image_byte_count);
        incoming_batch_events.push(next_incoming_event);
        if incoming_image_byte_count >= MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH {
            break;
        }
    }
    (incoming_batch_events, None)
}

/// Return the RGBA byte cost of one queued image-content event.
fn compute_incoming_image_byte_count(incoming_event: &Incoming) -> usize {
    match incoming_event {
        Incoming::Frame {
            session_event_result: Ok(SessionEvent::ImageContentChunk { image_chunk }),
            ..
        } => image_chunk.chunk_bytes.len(),
        Incoming::Frame { .. } | Incoming::Input(_) => 0,
    }
}

/// Open one connection into the session `session_selector` names in `home` and join it as
/// a client.
///
/// On this machine the session's endpoint file names the socket and holds the
/// token the Hello presents, and a display name is resolved by the router
/// first. A session with no endpoint file in the runtime directory is looked
/// up in the shared directory `koshi.kdl` in `config_directory` names, read
/// again on every dial. A session of this user's that refuses this build's
/// protocol version is dialed again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`](ipc_client::run_session_exchange_with_restart_wait)
/// states. On a server the whole admission runs — TLS with the
/// pinned certificate, the secret, and the scope check on the session asked
/// for — and the server resolves the name against the sessions that secret
/// reaches. A server whose router answers that it is restarting is dialed
/// again, as
/// [`dial_through_router_restart`](remote_client::dial_through_router_restart)
/// states, for up to [`RESTART_WINDOW_DURATION`].
fn dial_session(
    home: &Home,
    session_selector: &SessionSelector,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
    config_directory: Option<&Path>,
) -> Result<JoinedSession, CliError> {
    match home {
        Home::Local { runtime_directory } => {
            // The router turns a display name into a session's address before
            // this terminal joins it, and every dial after the first names the
            // session by id.
            let session_id = match session_selector {
                SessionSelector::SessionId(session_id) => *session_id,
                SessionSelector::SessionName(session_name) => {
                    lookup_session_address(runtime_directory, session_name)?.session_id
                }
            };
            ipc_client::run_session_exchange_with_restart_wait(
                runtime_directory,
                koshi_link::config::find_shared_sessions_base_directory(config_directory)
                    .as_deref(),
                session_id,
                None,
                |endpoint| {
                    let mut connection =
                        ipc_client::connect_to_session(endpoint, session_id, None)?;
                    let (client_id, session_id, resume_token) = join_session(
                        &mut connection,
                        &endpoint.connection_token,
                        None,
                        graphics_support,
                        cell_size,
                    )?;
                    let (reader, writer) = connection.split();
                    Ok(JoinedSession {
                        reader,
                        writer,
                        client_id,
                        session_id,
                        connection_token: endpoint.connection_token.clone(),
                        resume_token,
                    })
                },
            )
        }
        Home::Remote { server } => {
            remote_client::dial_through_router_restart(RESTART_WINDOW_DURATION, || {
                dial_remote(
                    server,
                    session_selector,
                    None,
                    None,
                    graphics_support,
                    cell_size,
                )
            })
            .map_err(CliError::from)
        }
    }
}

/// Dial `server`, ask it for the session `session_selector` names, and join that session
/// as a client.
///
/// The serving machine presents that session's endpoint token and writes the
/// Hello on this client's behalf, so the first frame read back is the session
/// server's answer to that Hello. The Attach after it is this client's own,
/// `resume_client_id` names the client record to come back as, and `resume_token` is the
/// secret the last attach minted, presented to get that attach's view back.
///
/// # Errors
/// [`DialError::Unreachable`] when the path to the server failed: the
/// connection could not be opened, or a frame of the join could not be written
/// or read. [`DialError::Restarting`] when the serving router refused the join
/// with [`koshi_ipc::router::ROUTER_RESTARTING_MESSAGE`]. [`DialError::Refused`]
/// when the server answered and every identical dial after it gets the same
/// answer: the certificate it presents is not the pinned one, it does not admit
/// the secret, the admitted secret does not reach `session_selector`, the
/// protocol versions do not overlap, or its answer is a frame this attach
/// cannot read.
fn dial_remote(
    server: &ServerReference,
    session_selector: &SessionSelector,
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> Result<JoinedSession, DialError> {
    // The join is held to JOIN_TIMEOUT_DURATION; the clock comes off once it is joined.
    let (link, saved_server) = remote_client::connect_saved_server(
        server,
        None,
        Some(remote_client::JOIN_TIMEOUT_DURATION),
    )?;
    let (mut reader, mut writer) =
        remote_client::attach_remote_session(link, session_selector.clone())
            .map_err(DialError::Unreachable)?;
    let hello_response = remote_client::read_forwarded_hello_answer(&mut reader, session_selector)?;
    validate_session_protocol_version(hello_response).map_err(DialError::Refused)?;
    writer
        .send(&build_attach_request(
            resume_client_id,
            resume_token,
            graphics_support,
            cell_size,
        ))
        .map_err(build_link_failure)?;
    let attach_response = reader.recv_answer().map_err(build_link_failure)?;
    let (client_id, session_id, minted_resume_token) =
        parse_attached_session(attach_response).map_err(DialError::Refused)?;

    // Joined: both halves block for as long as it takes from here.
    reader.set_deadline(None);
    writer.set_deadline(None);
    Ok(JoinedSession {
        reader,
        writer,
        client_id,
        session_id,
        connection_token: saved_server.connection_token,
        resume_token: minted_resume_token,
    })
}

/// The [`DialError`] a failed read or write on an open link to a server, or on
/// an open connection to a session, maps to. It carries what
/// [`talk::build_ipc_unavailable_error`] gives: [`DialError::Refused`] for
/// [`CliError::PreviousReleaseServer`], a server that koshi 0.1.0 to 0.4.0
/// started, and [`DialError::Unreachable`] for every other failure.
fn build_link_failure(ipc_error: IpcError) -> DialError {
    match talk::build_ipc_unavailable_error(ipc_error) {
        previous_release_refusal @ CliError::PreviousReleaseServer { .. } => {
            DialError::Refused(previous_release_refusal)
        }
        link_failure => DialError::Unreachable(link_failure),
    }
}

/// Come back into `session_id` after it said it is replacing its own process
/// image, and hand back the client this viewer came back as and the two halves
/// of the connection it came back on.
///
/// On this machine [`rejoin_session`] waits for the session's new socket, and
/// `connection_token` is stamped with the token that socket was advertised
/// under. For a session on another machine the whole dial runs again — no
/// endpoint file for that session exists on this machine — through
/// [`redial_restarted_remote_session_with`], until the serving machine reaches
/// the restarted session, the restarted session refuses this build's protocol
/// version, or [`RESTART_WINDOW_DURATION`] passes. Each dial waits
/// [`REMOTE_RESTART_POLL_INTERVAL_DURATION`] before it starts, the first dial
/// included.
///
/// `resume_token` is stamped with the secret the attach this client came back
/// on minted: the local rejoin records none, and a fresh dial of a server
/// records the one that dial minted.
///
/// The Attach presents no resume token either way: this client comes back by
/// naming `client_id`, and a session that still holds that record hands its
/// view straight back.
///
/// Returns the client the session answered with, then the two halves. That
/// client is `client_id` when the session handed its record back, and a fresh
/// client the session minted when it cannot: a restart that could not bring the
/// carried layout back holds no record, and a record whose tab closed is not
/// handed back.
///
/// `Err` carries the ending the attachment reports: what [`rejoin_session`]
/// answers on this machine, and what [`redial_restarted_remote_session_with`]
/// answers on a server.
///
/// On this machine the router's token is read from its endpoint file before the
/// rejoin starts. A rejoin that ends as
/// [`AttachmentEnding::RestartedIntoIncompatibleBuild`] returns only after
/// [`wait_for_router_restart`](router_client::wait_for_router_restart) sees another token, or after
/// [`RESTART_WINDOW_DURATION`] passes with no `koshi update` holding the update
/// lock. No router endpoint file means no wait.
fn reconnect_after_restart(
    home: &Home,
    session_id: SessionId,
    client_id: ClientId,
    connection_token: &mut ConnectionToken,
    resume_token: &mut Option<ConnectionToken>,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> Result<(ClientId, FrameReader, FrameWriter), AttachmentEnding> {
    match home {
        Home::Local { runtime_directory } => {
            let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
            let router_connection_token_before_restart =
                EndpointFile::load_from_path(&router_endpoint_path)
                    .ok()
                    .map(|router_endpoint| router_endpoint.connection_token);
            let (rejoined_client_id, endpoint, connection) = match rejoin_session(
                runtime_directory,
                session_id,
                client_id,
                connection_token,
                graphics_support,
                cell_size,
            ) {
                Ok(rejoined_session) => rejoined_session,
                Err(AttachmentEnding::RestartedIntoIncompatibleBuild) => {
                    if let Some(router_connection_token_before_restart) =
                        &router_connection_token_before_restart
                    {
                        let has_router_restarted = router_client::wait_for_router_restart(
                            runtime_directory,
                            router_connection_token_before_restart,
                            Instant::now() + RESTART_WINDOW_DURATION,
                        );
                        if !has_router_restarted {
                            tracing::warn!(
                                "the router did not restart inside the restart window; \
                                 attaching again through the router as it is"
                            );
                        }
                    }
                    return Err(AttachmentEnding::RestartedIntoIncompatibleBuild);
                }
                Err(restart_ending) => return Err(restart_ending),
            };
            *connection_token = endpoint.connection_token;
            *resume_token = None;
            let (reader, writer) = connection.split();
            Ok((rejoined_client_id, reader, writer))
        }
        Home::Remote { server } => {
            let joined = redial_restarted_remote_session_with(
                || {
                    dial_remote(
                        server,
                        &SessionSelector::SessionId(session_id),
                        Some(client_id),
                        None,
                        graphics_support,
                        cell_size,
                    )
                },
                Instant::now() + RESTART_WINDOW_DURATION,
            )?;
            *connection_token = joined.connection_token;
            *resume_token = joined.resume_token;
            Ok((joined.client_id, joined.reader, joined.writer))
        }
    }
}

/// Dial a session on a server again with `dial_connection` after it said it
/// is replacing its own process image, and hand back the session it joined.
///
/// The pause of [`REMOTE_RESTART_POLL_INTERVAL_DURATION`] comes before each
/// dial. A dial that fails is dialed again until `restart_deadline` passes. A
/// [`DialError::Refused`] carrying [`CliError::ProtocolVersionRefused`], which
/// says the restarted session speaks none of this build's protocol versions,
/// ends the dialing at once.
///
/// # Errors
/// [`AttachmentEnding::RefusedAfterRestart`] carrying the refused protocol
/// version, at once. After the first failed dial that ends past
/// `restart_deadline`: [`AttachmentEnding::RefusedAfterRestart`] carrying that
/// dial's refusal when it is a [`DialError::Refused`], and
/// [`AttachmentEnding::Restarting`] when it is a [`DialError::Unreachable`] or a
/// [`DialError::Restarting`].
fn redial_restarted_remote_session_with(
    mut dial_connection: impl FnMut() -> Result<JoinedSession, DialError>,
    restart_deadline: Instant,
) -> Result<JoinedSession, AttachmentEnding> {
    loop {
        thread::sleep(REMOTE_RESTART_POLL_INTERVAL_DURATION);
        let redial_error = match dial_connection() {
            Ok(joined) => return Ok(joined),
            Err(DialError::Refused(
                protocol_version_refusal @ CliError::ProtocolVersionRefused { .. },
            )) => {
                tracing::warn!(
                    %protocol_version_refusal,
                    "the restarted session refused this client's protocol version"
                );
                return Err(AttachmentEnding::RefusedAfterRestart(Box::new(
                    protocol_version_refusal,
                )));
            }
            Err(redial_error) => redial_error,
        };
        if Instant::now() < restart_deadline {
            continue;
        }
        return Err(match redial_error {
            DialError::Refused(redial_refusal) => {
                tracing::warn!(%redial_refusal, "the restarted session refused this client");
                AttachmentEnding::RefusedAfterRestart(Box::new(redial_refusal))
            }
            DialError::Unreachable(redial_failure) | DialError::Restarting(redial_failure) => {
                tracing::warn!(%redial_failure, "could not reach the restarted session");
                AttachmentEnding::Restarting
            }
        });
    }
}

/// Dial `server` for `session_id` again after a remote viewer's link dropped,
/// and hand back the connection it joined on.
///
/// The pause comes before each dial and widens as
/// [`compute_next_redial_wait`] says: 1 second, 2, 4, 8, then 8 before every dial after
/// that. A pause that would end past [`REDIAL_WINDOW_DURATION`] — 120 seconds from the
/// first pause — is not taken, and no dial follows it: the answer is the last
/// dial's cause. A dial already under way runs to its own timeout, so that
/// answer can arrive after the window closes.
///
/// A [`DialError::Refused`] ends this at once and is the answer: the server
/// answered, and every identical dial after it gets the same answer, so waiting
/// changes nothing. Only a [`DialError::Unreachable`] or a
/// [`DialError::Restarting`] is dialed again.
///
/// The pause is taken one second at a time. Each slice records
/// `Reconnecting { attempt, retry_in_seconds }` on `client` and draws the frame
/// already on the screen again through `screen`, so the tabline tag counts down
/// `RECONNECTING (attempt 1, retry in 1s)` before the first dial and
/// `attempt 2, retry in 2s` … `retry in 1s` before the second. `active_tab_id` is
/// the tab that frame shows, and `None` before any frame has been drawn. On
/// every answer, and before returning either way, `client` is put back to no
/// dialing under way; a dial that joined repaints once more, so the tag leaves
/// the screen before the new connection's first frame arrives.
///
/// `resume_token` is the secret the last attach minted. The session hands that
/// attach's view back for it — the active tab, each tab's focused and zoomed
/// pane, and each pane's scroll offset. The session drops that view 120 seconds
/// after it saw the link end, which is earlier than this window starts, so a
/// dial late in the window joins with a fresh view instead.
///
/// A session that no longer holds the view mints a fresh client, and the
/// returned [`JoinedSession`] names it: the viewer takes that client id as its own and
/// keeps running.
///
/// # Errors
/// The cause of the dial this gave up on: the refusal that ended it, or the last
/// unreachable or restarting cause before the window closed.
#[allow(clippy::too_many_arguments)]
fn redial_remote_session<B: Backend>(
    server: &ServerReference,
    session_id: SessionId,
    resume_token: Option<&ConnectionToken>,
    client: &mut Client,
    screen: &mut Screen<B>,
    active_tab_id: Option<TabId>,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> Result<JoinedSession, Box<CliError>> {
    redial_remote_session_with(
        || {
            dial_remote(
                server,
                &SessionSelector::SessionId(session_id),
                None,
                resume_token,
                graphics_support,
                cell_size,
            )
        },
        session_id,
        client,
        screen,
        active_tab_id,
    )
}

/// [`redial_remote_session`]'s loop over any dial: pause, paint the countdown,
/// call `dial_connection`, and classify its answer — a [`DialError::Refused`]
/// ends the loop at once, a
/// [`DialError::Unreachable`] or [`DialError::Restarting`] widens the pause and
/// dials again while the pause fits [`REDIAL_WINDOW_DURATION`].
///
/// # Errors
/// The cause of the dial this gave up on: the refusal that ended it, or the last
/// unreachable or restarting cause before the window closed.
fn redial_remote_session_with<B: Backend>(
    mut dial_connection: impl FnMut() -> Result<JoinedSession, DialError>,
    session_id: SessionId,
    client: &mut Client,
    screen: &mut Screen<B>,
    active_tab_id: Option<TabId>,
) -> Result<JoinedSession, Box<CliError>> {
    let redial_started_at = Instant::now();
    let mut retry_wait = FIRST_REDIAL_WAIT_DURATION;
    let mut redial_attempt: u32 = 1;
    let redial_cause = loop {
        let retry_second_count = u32::try_from(retry_wait.as_secs()).unwrap_or(u32::MAX);
        for retry_in_seconds in (1..=retry_second_count).rev() {
            client.set_reconnecting(Some(Reconnecting {
                attempt: redial_attempt,
                retry_in_seconds,
            }));
            screen.refresh(client, active_tab_id);
            thread::sleep(Duration::from_secs(1));
        }
        match dial_connection() {
            Ok(joined) => {
                client.set_reconnecting(None);
                screen.refresh(client, active_tab_id);
                return Ok(joined);
            }
            Err(DialError::Refused(redial_refusal)) => break redial_refusal,
            Err(DialError::Unreachable(redial_failure) | DialError::Restarting(redial_failure)) => {
                retry_wait = compute_next_redial_wait(retry_wait);
                redial_attempt += 1;
                if !can_redial_pause_fit(redial_started_at.elapsed(), retry_wait) {
                    break redial_failure;
                }
            }
        }
    };
    client.set_reconnecting(None);
    tracing::warn!(%redial_cause, %session_id, "could not join the session again");
    Err(Box::new(redial_cause))
}

/// Whether a pause of `pause_duration`, begun `elapsed_duration` after the first one, ends inside
/// [`REDIAL_WINDOW_DURATION`].
///
/// `elapsed_duration` 111 seconds with an 8-second `pause_duration` ends at
/// 119 and fits; 112 seconds with the same `pause_duration` ends at 120 and
/// does not.
fn can_redial_pause_fit(elapsed_duration: Duration, pause_duration: Duration) -> bool {
    elapsed_duration + pause_duration < REDIAL_WINDOW_DURATION
}

/// The wait one failed redial hands the next: `current_wait_duration` doubled, held at
/// [`MAX_REDIAL_WAIT_DURATION`].
///
/// From [`FIRST_REDIAL_WAIT_DURATION`] that walks 1 second → 2 → 4 → 8 → 8, and stays at
/// 8 seconds however many dials follow.
fn compute_next_redial_wait(current_wait_duration: Duration) -> Duration {
    (current_wait_duration * 2).min(MAX_REDIAL_WAIT_DURATION)
}

/// Read the terminal's size, record it on `client`, and report the viewport and
/// built-in pane area on `uplink`'s connection.
///
/// The `Resize` carries `Reported(viewport.rows - 2)`, with rows saturating at
/// zero. An `80x24` terminal therefore reports an `80x22` pane area.
///
/// `locally_measured_cell_size` is the cell measurement already in hand. A fresh
/// CSI 16t reply is requested only when it carries no usable pixel dimensions.
fn report_terminal_size_with_cell_size(
    client: &mut Client,
    uplink: &mut Uplink,
    cell_size_query: &mut terminal::CellSizeQuery,
    locally_measured_cell_size: Option<koshi_core::geometry::PixelCellSize>,
) {
    let viewport_size = get_terminal_viewport_size();
    let needs_cell_size_query =
        cell_size_query.update_cell_size_for_resize(locally_measured_cell_size);
    let current_cell_size = cell_size_query.get_current_cell_size();
    client.set_viewport_size(viewport_size);
    uplink.send_request(IpcRequestKind::Resize {
        viewport_size,
        pane_area: Some(compute_core_pane_area(viewport_size)),
        cell_size: current_cell_size,
    });
    if needs_cell_size_query {
        cell_size_query.request_cell_size();
    }
}

/// Take everything this terminal typed while the link was down off the loop's
/// channel and answer whether the terminal went away.
///
/// Every event on the channel is dropped, keys, pastes, mouse events and
/// resizes alike, and none is sent. A pending outer-terminal cell-size reply is
/// consumed by the query coordinator so its state remains ordered. Frames read
/// from the connection that broke are dropped too. The terminal's size is read
/// again once the new connection is up.
///
/// `true` when a [`RuntimeEvent::Quit`] was among them, which is this terminal
/// going away. The drain still runs to the end, so nothing typed before it is
/// left on the channel.
fn drop_input_from_the_blackout(
    incoming_receiver: &mpsc::Receiver<Incoming>,
    cell_size_query: &mut terminal::CellSizeQuery,
) -> bool {
    let mut is_terminal_gone = false;
    while let Ok(incoming_event) = incoming_receiver.try_recv() {
        let Incoming::Input(runtime_event) = incoming_event else {
            continue;
        };
        match *runtime_event {
            RuntimeEvent::CellSize { cell_size, .. } => {
                let _ = cell_size_query.accept_cell_size_reply(cell_size);
            }
            RuntimeEvent::Quit => is_terminal_gone = true,
            _ => {}
        }
    }
    is_terminal_gone
}

/// The session a listing settles on, picked from the sessions running for this
/// user and the rows in `remote`.
///
/// The local rows are the ones `koshi list-sessions` prints, from the same sweep
/// of the runtime directory, so nothing here probes anything that listing does
/// not. `remote` holds one row per session on a saved server that answered,
/// each carrying that server in its `server` field; a picker for a session
/// switch passes none, since a session on another machine is not one this
/// session can move a client to. A single local row is the answer on its own.
/// Every other non-empty listing — several rows, or one row on a saved server —
/// is printed and the number typed on stdin selects the row. This runs before
/// the terminal enters raw mode, so the prompt is a plain stdin read.
///
/// A session that is listening but could not answer leaves both "nothing is
/// running" and "this is the only one" unprovable, so a list of under two local
/// rows reports that session instead of settling on either.
///
/// The answer names where the picked row sat. The local rows come first and
/// `remote` follows, so a place past the local count is
/// [`SessionSelection::Remote`] at that many places into `remote`.
fn select_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    remote_session_rows: Vec<SessionRow>,
) -> Result<SessionSelection, CliError> {
    let discovered_sessions =
        discovery::fetch_all_session_overviews(runtime_directory, shared_sessions_base_directory);
    let mut session_rows = discovery::build_session_rows(&discovered_sessions.sessions);
    if session_rows.len() < 2 && !discovered_sessions.is_complete() {
        return Err(
            discovered_sessions.build_unanswered_error("cannot tell which session to attach to")
        );
    }
    let local_row_count = session_rows.len();
    session_rows.extend(remote_session_rows);
    if session_rows.is_empty() {
        return Err(CliError::NoSessions);
    }
    let selected_row_index =
        if is_single_local_session_selection(session_rows.len(), local_row_count) {
            0
        } else {
            parse_session_selection(&session_rows, &prompt_for_session_selection(&session_rows)?)?
        };
    Ok(match selected_row_index.checked_sub(local_row_count) {
        Some(remote_row_index) => SessionSelection::Remote(remote_row_index),
        None => SessionSelection::Local(session_rows[selected_row_index].session_id.to_string()),
    })
}

/// Whether a listing of `session_row_count` rows, the first
/// `local_session_row_count` of them on this machine, settles on its only row
/// without asking: exactly one row, and that row is local. A single remote row, and every longer listing, is asked.
fn is_single_local_session_selection(
    session_row_count: usize,
    local_session_row_count: usize,
) -> bool {
    session_row_count == 1 && local_session_row_count == 1
}

/// Which row a listing settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionSelection {
    /// A session running for this user on this machine, named by its id.
    Local(String),
    /// A session on a saved server: where in the listing's `remote` rows it sat.
    Remote(usize),
}

/// Where in `rows` a listing settles. A list of one row settles on it without
/// printing anything; a longer list is printed by [`prompt_for_session_selection`]
/// and the number typed
/// on stdin names the row.
///
/// # Errors
/// [`CliError::NoSessions`] for an empty `session_rows`. [`CliError::InvalidArgs`] when
/// stdin cannot be read, and when the line is not one of the listed numbers.
fn select_session_index(session_rows: &[SessionRow]) -> Result<usize, CliError> {
    match session_rows {
        [] => Err(CliError::NoSessions),
        [_] => Ok(0),
        many_session_rows => parse_session_selection(
            many_session_rows,
            &prompt_for_session_selection(many_session_rows)?,
        ),
    }
}

/// Print one numbered line per session — number, name, id, and for a session
/// on a saved server `(remote: <server>)` — and read back the line the user
/// answers with. The prompt names the range, `[1-3]`, or `[1]` for a single
/// row.
///
/// A line that cannot be read names the number that was expected.
fn prompt_for_session_selection(session_rows: &[SessionRow]) -> Result<String, CliError> {
    for (row_number, session_row) in session_rows.iter().enumerate() {
        match &session_row.server_name_or_address {
            Some(server_name_or_address) => {
                println!(
                    "{}) {} {} (remote: {server_name_or_address})",
                    row_number + 1,
                    session_row.session_name,
                    session_row.session_id
                );
            }
            None => println!(
                "{}) {} {}",
                row_number + 1,
                session_row.session_name,
                session_row.session_id
            ),
        }
    }
    let session_selection_range = match session_rows.len() {
        1 => String::from("1"),
        session_row_count => format!("1-{session_row_count}"),
    };
    print!("attach to which session? [{session_selection_range}] ");
    let _ = io::stdout().flush();
    let mut session_selection_line = String::new();
    io::stdin()
        .read_line(&mut session_selection_line)
        .map_err(|input_read_error| CliError::InvalidArgs {
            detail: format!(
                "expected a session number 1 to {}, and stdin could not be read: {input_read_error}",
                session_rows.len()
            ),
        })?;
    Ok(session_selection_line)
}

/// Where in `session_rows` a listing settles: the place the number on
/// `session_selection_line` names.
///
/// Empty `rows` is [`CliError::NoSessions`]; a number outside
/// `1..=rows.len()`, and a line that is not a number, are
/// [`CliError::InvalidArgs`] naming the range.
fn parse_session_selection(
    session_rows: &[SessionRow],
    session_selection_line: &str,
) -> Result<usize, CliError> {
    if session_rows.is_empty() {
        return Err(CliError::NoSessions);
    }
    let trimmed_session_selection = session_selection_line.trim();
    trimmed_session_selection
        .parse::<usize>()
        .ok()
        .and_then(|session_number| {
            let session_index = session_number.checked_sub(1)?;
            (session_index < session_rows.len()).then_some(session_index)
        })
        .ok_or_else(|| CliError::InvalidArgs {
            detail: format!(
                "`{trimmed_session_selection}` is not one of the listed sessions; \
                 expected a number 1 to {}",
                session_rows.len()
            ),
        })
}

/// Ask the router where the session `selector` names listens, starting a
/// router first when none is running.
///
/// A value that reads as a session id (`session-<uuid>` or a bare UUID) is
/// that id; anything else is a display name for the router to match.
fn lookup_session_address(
    runtime_directory: &Path,
    selector: &str,
) -> Result<SessionAddress, CliError> {
    let session_selector = build_session_selector(selector);
    match submit_router_request(
        runtime_directory,
        RouterRequestKind::AttachLookup { session_selector },
    )? {
        RouterResult::Found(address) => Ok(address),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(CliError::IpcUnavailable {
            detail: format!(
                "the router answered an attach lookup with {}",
                other.get_wire_name()
            ),
        }),
    }
}

/// What the user typed, as the selector both the router and a remote server
/// resolve: a `session-<uuid>` id or a bare UUID is that id, and anything else
/// is a display name for the far side to match.
fn build_session_selector(selector: &str) -> SessionSelector {
    match parse_prefixed_uuid(selector, "session") {
        Ok(uuid) => SessionSelector::SessionId(SessionId::from_uuid(uuid)),
        Err(_) => SessionSelector::SessionName(selector.to_string()),
    }
}

/// Join the session on an open connection: write the Hello and the Attach back
/// to back, then read both replies in order. Returns the client the server
/// minted for this terminal, the session it says that client joined, and the
/// secret this attach minted.
///
/// The client names no identity of its own — the server mints the client id
/// and answers with it — so every value comes from the reply.
///
/// `resume_client_id` names the client record to come back as: a client returning after
/// the session replaced its own process image carries it, and a first join
/// leaves it `None`. The server hands that record back when it still holds it,
/// the tab that record was viewing still exists, and no connection is
/// streaming for it, and mints a fresh client otherwise, so the returned id is
/// the one the caller holds from here either way.
///
/// The Attach presents no resume token: a join over a connection this machine
/// opened names the client record it comes back as instead.
///
/// # Errors
/// [`DialError::Unreachable`] when the Hello or the Attach could not be written,
/// or a reply could not be read. [`DialError::Refused`] for every reply that
/// arrived and does not join: a refused Hello, a protocol version this build
/// does not accept, a refused Attach, and an answer of another kind.
fn join_session(
    connection: &mut Connection,
    connection_token: &ConnectionToken,
    resume_client_id: Option<ClientId>,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> Result<(ClientId, SessionId, Option<ConnectionToken>), DialError> {
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(connection_token.clone()),
    };
    connection.send(&hello).map_err(build_link_failure)?;
    connection
        .send(&build_attach_request(
            resume_client_id,
            None,
            graphics_support,
            cell_size,
        ))
        .map_err(build_link_failure)?;

    validate_session_protocol_version(connection.recv_answer().map_err(build_link_failure)?)
        .map_err(DialError::Refused)?;
    parse_attached_session(connection.recv_answer().map_err(build_link_failure)?)
        .map_err(DialError::Refused)
}

/// The Attach this client writes, numbered 2: the request that follows the
/// Hello on every connection into a session.
///
/// `resume_client_id` names the client record to come back as, and is `None` on a first
/// join. `resume_token` is the secret the last attach minted, presented to get
/// that attach's view back, and is `None` on a first join and whenever no
/// token was minted. Reports the pane area left by the built-in two-row UI.
fn build_attach_request(
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> IpcRequest {
    let viewport_size = get_terminal_viewport_size();
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport_size,
            resume_client_id,
            resume_token: resume_token.cloned(),
            pane_area: Some(compute_core_pane_area(viewport_size)),
            graphics_capabilities: graphics_support.build_graphics_capabilities(),
            cell_size,
        },
    }
}

/// Check the protocol version a Hello answer settled on against the range this
/// build asked for.
///
/// A refusal is what [`build_peer_refusal_error`](talk::build_peer_refusal_error)
/// gives: [`CliError::ProtocolVersionRefused`] for
/// [`IpcErrorCode::UnsupportedVersion`](koshi_ipc::protocol::IpcErrorCode::UnsupportedVersion),
/// and [`CliError::IpcUnavailable`] for every other code.
fn validate_session_protocol_version(incoming_response: IncomingResponse) -> Result<(), CliError> {
    match talk::SESSION_PEER_WORDS.take_response_result(incoming_response)? {
        IpcResult::Hello {
            protocol_version, ..
        } => talk::SESSION_PEER_WORDS.validate_settled_protocol_version(protocol_version),
        IpcResult::Error(refusal) => Err(talk::build_peer_refusal_error(&refusal)),
        other => Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&other)),
    }
}

/// The client the server minted for this terminal, the session it says that
/// client joined, and the secret this attach minted, out of an Attach answer.
///
/// An answer that echoes no pane area is taken as it stands, and logs one
/// debug line.
fn parse_attached_session(
    incoming_response: IncomingResponse,
) -> Result<(ClientId, SessionId, Option<ConnectionToken>), CliError> {
    match talk::SESSION_PEER_WORDS.take_response_result(incoming_response)? {
        IpcResult::Attached {
            client_id,
            session_id,
            resume_token,
            pane_area,
            ..
        } => {
            if pane_area.is_none() {
                tracing::debug!("the session echoed no pane area in its attach answer");
            }
            Ok((client_id, session_id, resume_token))
        }
        IpcResult::Error(refusal) => Err(talk::build_peer_refusal_error(&refusal)),
        other => Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&other)),
    }
}

/// Come back to a session that is replacing its own process image: wait for its
/// new socket, connect to it, and join again as `client_id`. Returns the client
/// the session answered with, the endpoint file and the open connection.
///
/// The client is `client_id` when the session handed that record back, and a
/// fresh client the session minted otherwise.
///
/// `connection_token` is the token this client attached under; the wait watches it for a
/// change.
///
/// `Err` carries the ending the attachment reports.
/// [`AttachmentEnding::RestartedIntoIncompatibleBuild`] when the new socket
/// refuses this build's protocol version.
/// [`AttachmentEnding::RefusedAfterRestart`] carrying the refusal when the new
/// socket answers the join with any other refusal.
/// [`AttachmentEnding::Restarting`] for every other way the client cannot come
/// back: another local user's session, which advertises no endpoint file this
/// user can read; a session that has not come back inside
/// [`RESTART_WINDOW_DURATION`]; and a new socket that refuses the connection,
/// or that breaks before it answers the join.
fn rejoin_session(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    connection_token: &ConnectionToken,
    graphics_support: terminal::GraphicsSupport,
    cell_size: Option<koshi_core::geometry::PixelCellSize>,
) -> Result<(ClientId, EndpointFile, Connection), AttachmentEnding> {
    if connection_token.expose_secret().is_empty() {
        tracing::warn!(
            %session_id,
            "another local user's session is restarting, and this user cannot read its endpoint file"
        );
        return Err(AttachmentEnding::Restarting);
    }
    let deadline = Instant::now() + RESTART_WINDOW_DURATION;
    let Some(endpoint) = ipc_client::wait_for_new_session_endpoint(
        runtime_directory,
        session_id,
        connection_token,
        deadline,
    ) else {
        tracing::warn!(%session_id, "the session advertised no new socket after its restart");
        return Err(AttachmentEnding::Restarting);
    };
    let mut connection = ipc_client::connect_to_session(&endpoint, session_id, None).map_err(
        |connection_error| {
            tracing::warn!(%connection_error, "could not reach the restarted session");
            AttachmentEnding::Restarting
        },
    )?;
    let (rejoined_client_id, _, _) = join_session(
        &mut connection,
        &endpoint.connection_token,
        Some(client_id),
        graphics_support,
        cell_size,
    )
    .map_err(|join_error| match join_error {
        DialError::Refused(join_refusal @ CliError::ProtocolVersionRefused { .. }) => {
            tracing::warn!(%join_refusal, "the restarted session refused this client");
            AttachmentEnding::RestartedIntoIncompatibleBuild
        }
        DialError::Refused(join_refusal) => {
            tracing::warn!(%join_refusal, "the restarted session refused this client");
            AttachmentEnding::RefusedAfterRestart(Box::new(join_refusal))
        }
        DialError::Unreachable(join_failure) | DialError::Restarting(join_failure) => {
            tracing::warn!(%join_failure, "could not reach the restarted session");
            AttachmentEnding::Restarting
        }
    })?;
    Ok((rejoined_client_id, endpoint, connection))
}

/// This terminal's size in cells, or [`FALLBACK_VIEWPORT`] when it has none to
/// report.
fn get_terminal_viewport_size() -> Size {
    match size() {
        Ok((column_count, row_count)) => Size {
            column_count,
            row_count,
        },
        Err(size_read_error) => {
            tracing::warn!(%size_read_error, "could not read the terminal size");
            FALLBACK_VIEWPORT
        }
    }
}

/// Read the session's frames on their own thread and put each on the loop's
/// channel. A failed read is put there too and ends the thread: it is the
/// frame the loop classifies as a death.
///
/// A frame this build has no variant for is dropped here and never reaches the
/// loop. It comes from a newer session server, and the frames around it still
/// draw.
///
/// `connection_index` numbers the connection being read, and every frame carries it,
/// so the loop can tell this reader's frames from those of a connection it has
/// already left.
fn spawn_frame_reader(
    mut frame_reader: FrameReader,
    connection_index: u64,
    incoming_sender: mpsc::SyncSender<Incoming>,
) {
    let _ = thread::Builder::new()
        .name("koshi-attach-reader".to_string())
        .spawn(move || loop {
            let session_event_result = match frame_reader.recv::<IncomingEvent>() {
                Ok(MaybeKnown::Known(session_event)) => Ok(session_event),
                Ok(MaybeKnown::Unknown { variant_name }) => {
                    tracing::debug!(%variant_name, "session frame this build does not have");
                    continue;
                }
                Err(read_error) => Err(read_error),
            };
            let is_connection_broken = session_event_result.is_err();
            if incoming_sender
                .send(Incoming::Frame {
                    connection_index,
                    session_event_result,
                })
                .is_err()
                || is_connection_broken
            {
                break;
            }
        })
        .expect("spawn attach reader thread");
}

/// Write the loop's requests on their own thread and give back the queue they
/// are handed to.
///
/// A write blocks until the session has taken the bytes, so it is done here:
/// the loop only puts the request on the queue, and a session reading slowly
/// backs the queue up instead of holding this terminal's input.
///
/// Requests leave in the order they were queued and nothing here is folded or
/// reordered: a [`WireMouseAction::Forward`] is one report the pane's program
/// must see, and every request carries the `request_id` the session answers
/// under. The pile the loop holds is where folding happens, in
/// [`coalesce_mouse_actions`].
///
/// A request over the frame cap — a paste of more text than one frame carries —
/// is refused with nothing written, and that request alone is dropped; the next
/// one goes out. Any other failed write ends the thread.
fn spawn_uplink_writer(mut writer: FrameWriter) -> mpsc::Sender<IpcRequest> {
    let (request_sender, request_receiver) = mpsc::channel::<IpcRequest>();
    let _ = thread::Builder::new()
        .name("koshi-attach-writer".to_string())
        .spawn(move || {
            for ipc_request in request_receiver {
                match writer.send(&ipc_request) {
                    Ok(()) => {}
                    Err(IpcError::FrameTooLarge {
                        frame_byte_count,
                        maximum_frame_byte_count,
                    }) => {
                        tracing::warn!(
                            frame_byte_count,
                            maximum_frame_byte_count,
                            kind = ipc_request.request_kind.get_request_kind_name(),
                            "request over the cap was not sent"
                        );
                    }
                    Err(write_error) => {
                        tracing::warn!(%write_error, "could not send to the session");
                        break;
                    }
                }
            }
        })
        .expect("spawn attach writer thread");
    request_sender
}

/// Move every event the terminal-input thread produces onto the loop's own
/// channel, so the session's frames and this terminal's input arrive on one
/// receiver.
fn spawn_input_relay(
    input_receiver: mpsc::Receiver<RuntimeEvent>,
    incoming_sender: mpsc::SyncSender<Incoming>,
) {
    let _ = thread::Builder::new()
        .name("koshi-attach-input".to_string())
        .spawn(move || {
            for runtime_event in input_receiver {
                if incoming_sender
                    .send(Incoming::Input(Box::new(runtime_event)))
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn attach input relay thread");
}

/// Build the bounded queue shared by terminal input and the session reader.
fn build_incoming_channel() -> (mpsc::SyncSender<Incoming>, mpsc::Receiver<Incoming>) {
    mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)
}

/// Build the bounded queue between the terminal reader and the input relay.
fn build_input_channel() -> (mpsc::SyncSender<RuntimeEvent>, mpsc::Receiver<RuntimeEvent>) {
    mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)
}

/// Answer one event read from this terminal.
///
/// A key belongs to the viewer that received it: the keymap, the input mode and
/// any open sequence all live here, so this decides what the press means and
/// the session sees only the answer — the commands a binding runs, or the whole
/// key event to write. A release, and a key no keybinding can name, resolve
/// nothing and reach the session as they are. A press that goes to the pane's
/// program also ends the selection gesture under way, since the input is the
/// program's. A resize records the
/// viewport and pane area, since the session reconciles tab sizes from every
/// viewer's report. Pasted text goes up whole and ends the gesture too; the
/// session writes it into the pane, bracketing it when that pane asked for
/// bracketed paste. A paste of more text than one frame carries never leaves
/// this terminal.
///
/// [`RuntimeEvent::Quit`] never reaches here: the loop reads it as
/// [`AttachmentEnding::TerminalGone`] and stops. An input thread runs only for a terminal
/// that had keys to read, so a read failure from one is that terminal going
/// away.
///
/// `cell_size_query` holds the attachment's terminal query state: a resize
/// invalidates it and a cell-size reply settles it.
fn process_runtime_input_with_cell_size(
    client: &mut Client,
    uplink: &mut Uplink,
    cell_size_query: &mut terminal::CellSizeQuery,
    runtime_event: RuntimeEvent,
) {
    match runtime_event {
        RuntimeEvent::CellSize { cell_size, .. } => {
            let (reported_cell_size, needs_cell_size_query) =
                cell_size_query.accept_cell_size_reply(cell_size);
            if needs_cell_size_query {
                cell_size_query.request_cell_size();
            }
            if let Some(reported_cell_size) = reported_cell_size {
                uplink.send_request(IpcRequestKind::CellSize {
                    cell_size: reported_cell_size,
                });
            }
        }
        RuntimeEvent::KeyInput { key_input, .. } => {
            // A release has no binding chord. Placement owns releases while
            // active; every other release goes to the focused pane.
            let Some(chord) = key_input.to_binding_chord() else {
                if !client.is_placement_mode_active() {
                    uplink.send_request(IpcRequestKind::Keyboard { key_input });
                }
                return;
            };
            match client.resolve_key(chord, Instant::now()) {
                KeyOutcome::Fire(bound_action) => uplink.submit_bound_action(client, bound_action),
                KeyOutcome::PassThrough(_) => {
                    // The key belongs to the program in the pane, so a
                    // selection gesture over it is over.
                    client.end_mouse_selection();
                    uplink.send_request(IpcRequestKind::Keyboard { key_input });
                }
                // Held or dropped: nothing reaches the session. A chord that
                // opens or closes a sequence moves the breadcrumb the hint
                // bar draws. A discard moves nothing and draws nothing.
                KeyOutcome::Pending | KeyOutcome::Discard => {}
            }
        }
        RuntimeEvent::Resize {
            viewport_size,
            pane_area,
            cell_size,
            ..
        } => {
            let needs_cell_size_query = cell_size_query.update_cell_size_for_resize(cell_size);
            let cell_size = cell_size_query.get_current_cell_size();
            client.set_viewport_size(viewport_size);
            let pane_area = pane_area.unwrap_or_else(|| compute_core_pane_area(viewport_size));
            uplink.send_request(IpcRequestKind::Resize {
                viewport_size,
                pane_area: Some(pane_area),
                cell_size,
            });
            if needs_cell_size_query {
                cell_size_query.request_cell_size();
            }
        }
        RuntimeEvent::HostPaste { pasted_text, .. } => {
            if client.is_placement_mode_active() {
                return;
            }
            // The text belongs to the program in the pane, so a selection
            // gesture over it is over.
            client.end_mouse_selection();
            uplink.send_request(IpcRequestKind::Paste { pasted_text });
        }
        _ => {}
    }
}

/// Start the newest placement read retained while an earlier read was in flight.
fn send_next_queued_placement_read(client: &mut Client, uplink: &mut Uplink) {
    if let Some((source_pane_id, destination_tab_id)) = client.take_queued_placement_read() {
        uplink.send_placement_read(client, source_pane_id, destination_tab_id);
    }
}

/// Fire the viewer's open key sequence if its ambiguity deadline has passed at
/// `now`, sending the commands it resolves to up the connection.
///
/// A sequence that is both a complete binding and a longer one's prefix fires
/// when its deadline passes. The viewer holds it, so it decides; the session
/// only runs what comes back.
fn fire_expired_key_sequence(client: &mut Client, uplink: &mut Uplink, current_time: Instant) {
    let Some(bound_action) = client.expire_key_sequence(current_time) else {
        return;
    };
    uplink.submit_bound_action(client, bound_action);
}

/// Answer one mouse event with pane placement, against `mouse_frame`, the frame
/// this terminal last painted. Returns `true` when placement took the event and
/// sent what it decided through `uplink`, and `false` when the event takes the
/// normal mouse path. An unshifted left press on a placement handle opens
/// placement; see [`Client::handle_placement_mouse`] for every other case.
fn handle_placement_mouse_event(
    client: &mut Client,
    uplink: &mut Uplink,
    mouse_frame: &MouseFrame,
    mouse_input: MouseInput,
) -> bool {
    let Some(placement_input_action) =
        client.handle_placement_mouse(mouse_input, mouse_frame, Instant::now())
    else {
        return false;
    };
    uplink.submit_placement_input_action(client, placement_input_action);
    true
}

/// Answer one mouse event read from this terminal against `mouse_frame`, the
/// frame this terminal last painted, and add everything the viewer decided to
/// `pending_mouse_actions` through [`queue_mouse_actions`].
///
/// Viewer state moves at once — the hovered pane, the gesture under way, the
/// capture a press takes — so a drag keeps tracking the pointer while earlier
/// rounds are still unanswered. Every [`MouseAction::Forward`] carrying a press
/// records the capture here, through [`Client::note_press_forwarded`], before
/// the round is written.
fn handle_mouse_event(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    mouse_input: MouseInput,
    pending_mouse_actions: &mut Vec<MouseAction>,
) {
    let mouse_actions = client.handle_mouse(mouse_input, mouse_frame, Instant::now());
    for mouse_action in &mouse_actions {
        if let MouseAction::Forward {
            pane_id,
            mouse_input,
        } = mouse_action
        {
            if let MouseKind::Press(button) = mouse_input.mouse_kind {
                client.note_press_forwarded(*pane_id, button);
            }
        }
    }
    queue_mouse_actions(pending_mouse_actions, mouse_actions);
}

/// Add `mouse_actions` to the pile waiting for the next write and keep the pile at
/// [`MAX_PENDING_MOUSE_ACTION_COUNT`].
///
/// A pile over the cap is folded first, which loses nothing: the fold states
/// the same movement in fewer actions. What is still over the cap after that is
/// trimmed by dropping the oldest scrolls, which move this viewer's own view of
/// a pane and nothing else, so an overrun costs scrollback distance on the
/// oldest wheel ticks of the burst.
///
/// Nothing else is ever dropped. A [`MouseAction::Forward`] is one report the
/// pane's program must see, and [`MouseAction::AlternateScrollArrows`] is arrow keys
/// that program reads; a [`MouseAction::Command`] runs once, and a
/// [`MouseAction::Resize`] states a border's whole distance from its drag
/// anchor. A pile holding no scrolls therefore stays over the cap rather than
/// break any of those.
fn queue_mouse_actions(
    pending_mouse_actions: &mut Vec<MouseAction>,
    mouse_actions: Vec<MouseAction>,
) {
    pending_mouse_actions.extend(mouse_actions);
    if pending_mouse_actions.len() <= MAX_PENDING_MOUSE_ACTION_COUNT {
        return;
    }
    *pending_mouse_actions = coalesce_mouse_actions(take(pending_mouse_actions));
    let mut excess_scroll_action_count = pending_mouse_actions
        .len()
        .saturating_sub(MAX_PENDING_MOUSE_ACTION_COUNT);
    pending_mouse_actions.retain(|mouse_action| {
        let should_drop_scroll =
            excess_scroll_action_count > 0 && matches!(mouse_action, MouseAction::Scroll { .. });
        excess_scroll_action_count -= usize::from(should_drop_scroll);
        !should_drop_scroll
    });
}

/// Take in what the session did with one round of mouse actions. An answer
/// releases nothing — it only reconciles this viewer's state with what the
/// session did.
///
/// Every answer says which gesture it belongs to on its own: a `Scrolled` names
/// its pane and a `Resized` names its pane, side and step. Answers are therefore
/// applied whatever order they arrive in, and an answer for a gesture that has
/// since ended changes nothing.
///
/// A wheel tick's `Scrolled` is applied too and does nothing: `Client`'s
/// `selection_scroll_origin_row_index` is written only
/// by `Client::expire_mouse_scroll`, so only a scroll the edge timer asked for
/// finds anything there.
///
/// A `Resized` does three things for the border it names. It moves that border's
/// drag anchor over the cells the session took — an answer for a border the
/// viewer has since let go of leaves the drag alone, which
/// `Client::note_resize_applied` checks. It re-bases the buffered moves for that
/// same border, which were measured from the anchor this answer advances, so the
/// cells the session already took come off them. And it forgets the
/// [`SentBorderMove`] the round recorded, so those cells stop coming off the
/// next move for that border.
fn apply_mouse_answers(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    sent_border_moves: &mut Vec<SentBorderMove>,
    request_id: u64,
    mouse_answers: Vec<MouseAnswer>,
    pending_mouse_actions: &mut Vec<MouseAction>,
) {
    for mouse_answer in mouse_answers {
        match mouse_answer {
            MouseAnswer::Scrolled {
                pane_id,
                top_row_number: reported_top_row_index,
            } => {
                queue_mouse_actions(
                    pending_mouse_actions,
                    client.note_scroll_applied(pane_id, reported_top_row_index, mouse_frame),
                );
            }
            MouseAnswer::Resized {
                pane_id,
                border_side,
                resize_step,
                applied_cell_count,
            } => {
                client.note_resize_applied(pane_id, border_side, resize_step, applied_cell_count);
                rebase_border_moves(
                    pending_mouse_actions,
                    pane_id,
                    border_side,
                    resize_step,
                    applied_cell_count,
                );
                // The oldest match is the one this answer reports: the session
                // answers a round's moves in the order they were written.
                if let Some(sent_border_move_index) =
                    sent_border_moves.iter().position(|sent_border_move| {
                        sent_border_move.request_id == request_id
                            && sent_border_move.pane_id == pane_id
                            && sent_border_move.border_side == border_side
                    })
                {
                    sent_border_moves.remove(sent_border_move_index);
                }
            }
        }
    }
}

/// Take the cells the session just moved off every buffered border move for
/// `pane_id`'s `border_side`.
///
/// A buffered move names the whole distance from the drag anchor to the pointer
/// it was decided position, and the answered move travelled `applied_cell_count`
/// cells of that same distance in the direction `resize_step` names. What is
/// left to ask for is therefore each buffered move's own signed distance minus
/// `resize_step * applied_cell_count`,
/// which is zero when the session already went the whole way and flips sign when
/// the pointer crossed back past the border.
///
/// A drag that buffered 4 cells down while a round of 3 cells down was out is
/// left asking for 1. A buffered move for any other pane or side is left as it
/// is: it measures from its own anchor, which this answer did not move.
fn rebase_border_moves(
    pending_mouse_actions: &mut [MouseAction],
    pane_id: PaneId,
    border_side: Direction,
    resize_step: i16,
    applied_cell_count: u16,
) {
    let applied_cell_delta = i32::from(resize_step) * i32::from(applied_cell_count);
    for mouse_action in pending_mouse_actions {
        let MouseAction::Resize {
            pane_id: moved_pane_id,
            border_side: moved_border_side,
            resize_step: pending_resize_step,
            requested_cell_count,
        } = mouse_action
        else {
            continue;
        };
        if *moved_pane_id != pane_id || *moved_border_side != border_side {
            continue;
        }
        let remaining_cell_delta =
            i32::from(*pending_resize_step) * i32::from(*requested_cell_count) - applied_cell_delta;
        *pending_resize_step = if remaining_cell_delta < 0 { -1 } else { 1 };
        *requested_cell_count =
            u16::try_from(remaining_cell_delta.unsigned_abs()).unwrap_or(u16::MAX);
    }
}

/// Hand the pile to the writer thread, whatever else is already out there. The
/// one place anything mouse-shaped is sent.
///
/// Nothing is waited for and nothing is timed: the pile is only ever what one
/// pass of the loop decided, so a link fast enough to keep the loop level sends
/// the single action that woke it and folds nothing.
///
/// A border move names the whole distance from its drag anchor, so the cells the
/// moves already on the wire asked for come off it here, and so do the cells
/// this round's own earlier moves for that same border ask for: the session
/// travels every move in a round, one after another. Each move that survives
/// that is recorded in `sent_border_moves` under the `request_id` it went out with. A move
/// left with no cells to travel is dropped after the fold rather than before it,
/// so the fold still sees it and keeps it as the newest of the border's moves.
fn flush_mouse_round(
    uplink: &mut Uplink,
    sent_border_moves: &mut Vec<SentBorderMove>,
    pending_mouse_actions: &mut Vec<MouseAction>,
) {
    if pending_mouse_actions.is_empty() {
        return;
    }
    let mut mouse_round = coalesce_mouse_actions(take(pending_mouse_actions));
    let mut resize_moves: Vec<(PaneId, Direction, i32)> = Vec::new();
    for mouse_action in &mut mouse_round {
        if let MouseAction::Resize {
            pane_id,
            border_side,
            resize_step,
            requested_cell_count,
        } = mouse_action
        {
            let current_round_cell_delta: i32 = resize_moves
                .iter()
                .filter(|(moved_pane_id, moved_border_side, _)| {
                    *moved_pane_id == *pane_id && *moved_border_side == *border_side
                })
                .map(|(_, _, requested_cell_delta)| requested_cell_delta)
                .sum();
            let remaining_cell_delta = i32::from(*resize_step) * i32::from(*requested_cell_count)
                - compute_sent_border_cell_delta(sent_border_moves, *pane_id, *border_side)
                - current_round_cell_delta;
            *resize_step = if remaining_cell_delta < 0 { -1 } else { 1 };
            *requested_cell_count =
                u16::try_from(remaining_cell_delta.unsigned_abs()).unwrap_or(u16::MAX);
            if *requested_cell_count != 0 {
                resize_moves.push((
                    *pane_id,
                    *border_side,
                    i32::from(*resize_step) * i32::from(*requested_cell_count),
                ));
            }
        }
    }
    mouse_round.retain(|mouse_action| {
        !matches!(
            mouse_action,
            MouseAction::Resize {
                requested_cell_count: 0,
                ..
            }
        )
    });
    let Some(request_id) = send_mouse_round(uplink, mouse_round) else {
        return;
    };
    sent_border_moves.extend(resize_moves.into_iter().map(
        |(pane_id, border_side, requested_cell_delta)| SentBorderMove {
            request_id,
            pane_id,
            border_side,
            requested_cell_delta,
        },
    ));
    // A session that stops answering never trims this, so the oldest entries go
    // once it holds a burst's worth. A forgotten move leaves its cells counted
    // as never asked, so the next move for that border asks for them again.
    if sent_border_moves.len() > MAX_PENDING_MOUSE_ACTION_COUNT {
        sent_border_moves.drain(..sent_border_moves.len() - MAX_PENDING_MOUSE_ACTION_COUNT);
    }
}

/// The signed cells every written-and-unanswered move for `pane_id`'s `border_side` has
/// already asked for, positive to grow the pane.
///
/// Two unanswered moves that each grow the pane by 3 cells come to 6, so a
/// third move naming 7 cells of travel from the drag anchor asks for 1.
fn compute_sent_border_cell_delta(
    sent_border_moves: &[SentBorderMove],
    pane_id: PaneId,
    border_side: Direction,
) -> i32 {
    sent_border_moves
        .iter()
        .filter(|sent_border_move| {
            sent_border_move.pane_id == pane_id && sent_border_move.border_side == border_side
        })
        .map(|sent_border_move| sent_border_move.requested_cell_delta)
        .sum()
}

/// Send `mouse_actions` as one request and give back the `request_id` it went out
/// under, or `None` for an empty list, which is written as nothing at all.
///
/// One round, one id, one answer: the whole list travels as a single
/// [`IpcRequestKind::Mouse`], which the session answers exactly once.
fn send_mouse_round(uplink: &mut Uplink, mouse_actions: Vec<MouseAction>) -> Option<u64> {
    if mouse_actions.is_empty() {
        return None;
    }
    let mouse_round: Vec<WireMouseAction> = mouse_actions
        .into_iter()
        .map(convert_mouse_action_to_wire)
        .collect();
    Some(uplink.send_request(IpcRequestKind::Mouse(mouse_round)))
}

/// The wire spelling of one action the viewer decided, variant for variant.
fn convert_mouse_action_to_wire(mouse_action: MouseAction) -> WireMouseAction {
    match mouse_action {
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up,
            scroll_line_count,
        } => WireMouseAction::Scroll {
            pane_id,
            is_scrolling_up,
            scroll_line_count,
        },
        MouseAction::Forward {
            pane_id,
            mouse_input,
        } => WireMouseAction::Forward {
            pane_id,
            mouse_input,
        },
        MouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up,
            arrow_count,
        } => WireMouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up,
            arrow_count,
        },
        MouseAction::Resize {
            pane_id,
            border_side,
            resize_step,
            requested_cell_count,
        } => WireMouseAction::Resize {
            pane_id,
            border_side,
            resize_step,
            requested_cell_count,
        },
        MouseAction::Command(command) => WireMouseAction::Command(command),
    }
}

/// Fold a pile of actions into the shortest run that means the same thing.
///
/// Only neighbours fold, so an action of another kind between two foldable ones
/// keeps them apart: a scroll, a forward, then a scroll stays three actions.
///
/// - Two scrolls over one pane in one direction become one scroll of the summed
///   `scroll_line_count`: the session moves the view by a count, so two counts are their
///   sum.
/// - Two alternate-scroll runs over one pane in one direction become one run of
///   the summed `arrow_count`: the pane's program receives arrow keys, so two runs are
///   the same keys back to back.
/// - Two selection changes for one pane keep the newer: each carries the whole
///   highlight — kind, anchor and cursor — so the newer states all of it.
/// - Two border moves for one pane and side keep the newer: every buffered move
///   carries the full distance from the drag anchor to the pointer it was
///   decided at — an answer that advances the anchor re-bases the older ones in
///   [`rebase_border_moves`] — so they all measure from one place and the newest
///   is the whole move.
/// - A forward always pushes: each report is a separate event the pane's program
///   must see.
fn coalesce_mouse_actions(mouse_actions: Vec<MouseAction>) -> Vec<MouseAction> {
    let mut folded_mouse_actions: Vec<MouseAction> = Vec::with_capacity(mouse_actions.len());
    for mouse_action in mouse_actions {
        let unfolded_mouse_action = match folded_mouse_actions.last_mut() {
            Some(tail_mouse_action) => fold_mouse_action(tail_mouse_action, mouse_action),
            None => Some(mouse_action),
        };
        if let Some(mouse_action) = unfolded_mouse_action {
            folded_mouse_actions.push(mouse_action);
        }
    }
    folded_mouse_actions
}

/// Fold `next_mouse_action` into `tail_mouse_action` when the two state one
/// movement twice, and give back `next_mouse_action` when they do not.
fn fold_mouse_action(
    tail_mouse_action: &mut MouseAction,
    next_mouse_action: MouseAction,
) -> Option<MouseAction> {
    match (tail_mouse_action, next_mouse_action) {
        (
            MouseAction::Scroll {
                pane_id,
                is_scrolling_up,
                scroll_line_count,
            },
            MouseAction::Scroll {
                pane_id: next_pane_id,
                is_scrolling_up: next_is_scrolling_up,
                scroll_line_count: next_scroll_line_count,
            },
        ) if *pane_id == next_pane_id && *is_scrolling_up == next_is_scrolling_up => {
            *scroll_line_count += next_scroll_line_count;
            None
        }
        (
            MouseAction::AlternateScrollArrows {
                pane_id,
                is_scrolling_up,
                arrow_count,
            },
            MouseAction::AlternateScrollArrows {
                pane_id: next_pane_id,
                is_scrolling_up: next_is_scrolling_up,
                arrow_count: next_arrow_count,
            },
        ) if *pane_id == next_pane_id && *is_scrolling_up == next_is_scrolling_up => {
            *arrow_count += next_arrow_count;
            None
        }
        (
            MouseAction::Resize {
                pane_id,
                border_side,
                resize_step,
                requested_cell_count,
            },
            MouseAction::Resize {
                pane_id: next_pane_id,
                border_side: next_border_side,
                resize_step: next_resize_step,
                requested_cell_count: next_requested_cell_count,
            },
        ) if *pane_id == next_pane_id && *border_side == next_border_side => {
            *resize_step = next_resize_step;
            *requested_cell_count = next_requested_cell_count;
            None
        }
        (MouseAction::Command(held_command), MouseAction::Command(newer_command))
            if matches!(
                (held_command.as_ref(), newer_command.as_ref()),
                (
                    Command::Visual(VisualCommand::SetSelection(held_selection_args)),
                    Command::Visual(VisualCommand::SetSelection(newer_selection_args)),
                ) if held_selection_args.pane_id == newer_selection_args.pane_id
            ) =>
        {
            *held_command = newer_command;
            None
        }
        (_, next_mouse_action) => Some(next_mouse_action),
    }
}

/// The sooner of two deadlines, or `None` when neither is set.
fn select_earliest_duration(
    first_duration: Option<Duration>,
    second_duration: Option<Duration>,
) -> Option<Duration> {
    [first_duration, second_duration]
        .into_iter()
        .flatten()
        .min()
}

/// Classify one frame read from the event stream. `None` keeps the loop
/// reading; `Some` ends it.
///
/// Any failure to read is the same ending: the peer closing the socket — which
/// is what a session server exiting or being killed does — surfaces as a read
/// error, so no timeout is involved.
fn classify_session_event(
    session_event: &Result<SessionEvent, IpcError>,
) -> Option<AttachmentEnding> {
    match session_event {
        Ok(SessionEvent::Detached) => Some(AttachmentEnding::Detached),
        Ok(SessionEvent::Quit) => Some(AttachmentEnding::SessionEnded),
        Ok(SessionEvent::Restarting) => Some(AttachmentEnding::Restarting),
        Ok(SessionEvent::SwitchTo { session_id }) => {
            Some(AttachmentEnding::SwitchSession(*session_id))
        }
        Ok(_) => None,
        Err(_) => Some(AttachmentEnding::ConnectionDied),
    }
}

/// Print how the stream ended and hand back both the process outcome and the
/// session to attach to next: a broken connection names the cause and how to
/// reattach, and exits non-zero; a switch names the session and prints
/// nothing.
///
/// The way back names the machine the session runs on, so a session on another
/// machine names that server rather than this one.
///
/// A restart reaches here only when the client could not come back on the
/// session's new socket. It says the session restarted, then names the same
/// way back. A restart whose new image refused this client prints `the session
/// restarted and refused this client: <refusal>`, then the same way back. On
/// this machine, a restart into a build that refused this client's protocol
/// version runs `koshi attach <session id>` from `client_executable_path`
/// instead, through [`attach_again_from_client_executable`].
///
/// A remote viewer that gave up dialing again names the cause it gave up on,
/// then `the session continues without you`, then that same way back.
fn report_attachment_ending(
    home: &Home,
    ending: AttachmentEnding,
    session_id: SessionId,
    client_executable_path: Option<&Path>,
) -> Result<Option<SessionId>, CliError> {
    match ending {
        AttachmentEnding::Detached => {
            println!("detached from session {session_id}");
            Ok(None)
        }
        AttachmentEnding::SessionEnded => {
            println!("the session ended");
            Ok(None)
        }
        AttachmentEnding::SwitchSession(next_session_id) => Ok(Some(next_session_id)),
        AttachmentEnding::ConnectionDied => Err(CliError::Runtime {
            detail: format!(
                "the session ended unexpectedly\n  {}",
                build_reattach_instructions(home, session_id)
            ),
        }),
        AttachmentEnding::Restarting => Err(CliError::Runtime {
            detail: format!(
                "the session restarted and this client could not reconnect to it\n  {}",
                build_reattach_instructions(home, session_id)
            ),
        }),
        AttachmentEnding::RestartedIntoIncompatibleBuild => {
            attach_again_from_client_executable(home, session_id, client_executable_path)
        }
        AttachmentEnding::RefusedAfterRestart(refusal) => Err(CliError::Runtime {
            detail: format!(
                "the session restarted and refused this client: {refusal}\n  {}",
                build_reattach_instructions(home, session_id)
            ),
        }),
        AttachmentEnding::LinkLost(cause) => Err(CliError::Runtime {
            detail: format!(
                "{cause}\n  the session continues without you\n  {}",
                build_reattach_instructions(home, session_id)
            ),
        }),
        // Nothing is left to read a message, so this ending is logged rather
        // than printed. The session drops this client when the connection
        // closes behind it.
        AttachmentEnding::TerminalGone => {
            tracing::info!(%session_id, "this terminal went away; leaving the session running");
            Ok(None)
        }
    }
}

/// Run `koshi attach <session id>` from `client_executable_path` in this
/// terminal, after the session restarted into a build that refused this
/// client's protocol version.
///
/// Prints `koshi: the session restarted into another koshi build; attaching to
/// session <session id> again with <path>` on stderr first. On Unix the command
/// replaces this process's image and keeps its process id. On Windows the
/// command runs as a child in this console, and this process exits with the
/// child's exit code once the child ends, or with 1 when the child reports no
/// code.
///
/// # Errors
/// [`CliError::Runtime`] naming the restart, why the command did not start, and
/// the way back from [`build_reattach_instructions`]: when
/// `client_executable_path` is `None`, and when the command could not start.
fn attach_again_from_client_executable(
    home: &Home,
    session_id: SessionId,
    client_executable_path: Option<&Path>,
) -> Result<Option<SessionId>, CliError> {
    let start_failure_reason = match client_executable_path {
        None => String::from("this client could not read the path it was started from"),
        Some(client_executable_path) => {
            eprintln!(
                "koshi: the session restarted into another koshi build; \
                 attaching to session {session_id} again with {}",
                client_executable_path.display()
            );
            let start_error = run_attach_command(client_executable_path, session_id);
            format!(
                "could not start {}: {start_error}",
                client_executable_path.display()
            )
        }
    };
    Err(CliError::Runtime {
        detail: format!(
            "the session restarted into another koshi build and this client could not \
             reconnect to it: {start_failure_reason}\n  {}",
            build_reattach_instructions(home, session_id)
        ),
    })
}

/// The command `koshi attach <session id>`, run from `client_executable_path`
/// with this process's environment minus the
/// [`IN_SESSION_MARKER_VARIABLE_NAME`](koshi_link::in_session::IN_SESSION_MARKER_VARIABLE_NAME)
/// variable. A client that runs inside a pane of another session attaches to
/// `session_id` as a client, and does not move that pane's own client.
fn build_attach_command(
    client_executable_path: &Path,
    session_id: SessionId,
) -> std::process::Command {
    let mut attach_command = std::process::Command::new(client_executable_path);
    attach_command
        .arg("attach")
        .arg(session_id.to_string())
        .env_remove(koshi_link::in_session::IN_SESSION_MARKER_VARIABLE_NAME);
    attach_command
}

/// Replace this process's image with [`build_attach_command`]'s command, after
/// standard output is flushed. The command keeps this terminal, working
/// directory and process id, and this environment minus the variable
/// [`build_attach_command`] removes. Returns only the error that kept the
/// command from starting, on the terms
/// [`exec_and_keep_ignoring_sigpipe`](koshi_link::process::exec_and_keep_ignoring_sigpipe)
/// states.
#[cfg(unix)]
fn run_attach_command(client_executable_path: &Path, session_id: SessionId) -> io::Error {
    let _ = io::stdout().flush();
    koshi_link::process::exec_and_keep_ignoring_sigpipe(&mut build_attach_command(
        client_executable_path,
        session_id,
    ))
}

/// Run [`build_attach_command`]'s command as a child that shares this console,
/// after standard output is flushed, and exit this process with the child's
/// exit code once the child ends, or with 1 when the child reports no code.
/// Returns only the error that kept the child from starting.
#[cfg(windows)]
fn run_attach_command(client_executable_path: &Path, session_id: SessionId) -> io::Error {
    let _ = io::stdout().flush();
    match build_attach_command(client_executable_path, session_id).status() {
        Ok(child_exit_status) => std::process::exit(child_exit_status.code().unwrap_or(1)),
        Err(spawn_error) => spawn_error,
    }
}

/// How to reach `session_id` again from where it runs: the command that shows
/// whether it still runs, and the attach command to come back on.
///
/// A session on this machine reads `run \`koshi list-sessions\`; …`. One on a
/// server names that server in both commands — `koshi attach --remote my-box`
/// shows that server's sessions, since `list-sessions` answers from this
/// machine alone.
fn build_reattach_instructions(home: &Home, session_id: SessionId) -> String {
    match home {
        Home::Local { .. } => format!(
            "run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        ),
        Home::Remote { server } => {
            let server_label = server.format_server_label();
            format!(
                "run `koshi attach --remote {server_label}` to see that server's sessions; \
                 if session {session_id} is among them, reattach with \
                 `koshi attach --remote {server_label} {session_id}`"
            )
        }
    }
}
