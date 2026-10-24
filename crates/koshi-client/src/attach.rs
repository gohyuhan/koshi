//! The attached client: join a running session and become a second window onto
//! it.
//!
//! A session runs in one of two homes, and the home is picked before the first
//! connection opens. A session on this machine is joined over its control
//! socket: the router turns the value after `koshi attach` into that session's
//! address, starting a router first when none runs, and the session's endpoint
//! file holds the token the Hello presents. The Hello and a `Prepare` carrying
//! the Attach are written back to back, and a `Commit` runs the Attach once
//! the session answers `Prepared`, so joining costs two round trips.
//!
//! A session on another machine is joined over TLS through the server serving
//! it: `koshi attach --remote <server> [session]` presents the secret that
//! server saved, only a certificate with the fingerprint saved for that server
//! is accepted, and the server resolves the session against the sessions that
//! secret reaches. The server presents that session's endpoint token and writes
//! the Hello on this client's behalf. From the Attach on, both homes speak the
//! same frames on the same loop.
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
//! the same way. Keys and pastes typed while the client comes back are held in
//! the order typed and sent after the size report on the new link. The hold
//! keeps at most 1024 inputs and one frame of paste text: past either, every
//! input typed during that restart is dropped. While dock/undock mode, pane
//! placement mode, the move chooser, or floating pane move mode is on, nothing
//! is held.
//!
//! A viewer of a session on a server sends a `Ping` every third of
//! `remote-link-timeout-ms` while it waits for the session's answer to its
//! `Attach`. Once joined, its first request goes up at once: a `Ping` on the
//! first link, the size report on each link it gets back. A `Ping` then
//! follows every third of `remote-link-timeout-ms` after the last one. The
//! session writes a `Pong` whenever it has written the viewer nothing for a
//! third of `remote-link-timeout-ms`. A viewer that reads nothing on the link
//! for `remote-link-timeout-ms` counts the link as dropped. A write to the
//! session that puts no byte on the link for `remote-link-timeout-ms` ends the
//! thread that writes the link, with the requests it holds. A paste that takes
//! longer than that keeps the link while each socket write puts bytes on it,
//! and the session's `Pong` frames keep reaching the viewer meanwhile.
//! Example: with the default `30000`, a laptop that sleeps, or a network that
//! changes under the link, drops it 30 seconds after the last bytes read. A
//! link to a session on this machine sends no `Ping` and has no limit.
//!
//! A dropped link to a session on a server is not a way out either, while
//! `remote-reconnect` is on. The viewer draws
//! `RECONNECTING (attempt 1, retry in 1s)` on its tab strip, counting the
//! seconds down to each dial, and `RECONNECTING (attempt 1, dialing)` while a
//! dial runs. It dials that server again — after 1 second, then 2, 4, 8, and 8
//! seconds before every dial after that — until it joins or 120 seconds pass.
//! A dial the server answers with a refusal — a certificate that
//! is not the expected one, a secret it does not admit, a session that secret does
//! not reach, or a protocol version this build does not accept — is not dialed
//! again, and the viewer stops there.
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
//! The last request on a link this viewer leaves is `Leaving`. Its
//! `is_returning` is `true` when the viewer dials again or comes back after a
//! restart. It is `false` when this terminal goes away or the session moves
//! this viewer to another session: the viewer then waits up to 1 second for
//! the writer thread to hand it to the socket. If the quit keybinding or this
//! terminal going away ends the viewer before it takes the link that a rejoin
//! attempt opens, that link gets `Leaving` with `is_returning` `true`, then
//! closes. The first link of a terminal that cannot be set up does the same.
//! This terminal goes away when a read of its input fails, as a read
//! of a closed terminal does, and on the console close on Windows. On Linux
//! and macOS, `SIGHUP`, `SIGTERM`, `SIGINT` and `SIGQUIT` end the viewer the
//! same way, and once the terminal is restored the process ends by the
//! default action of the last of them that arrived. On Windows, a Ctrl+C or
//! Ctrl+Break signal ends the viewer the same way, and the process then exits
//! with `STATUS_CONTROL_C_EXIT` (`0xC000013A`). A second such signal before
//! the terminal is restored ends the process at once. A signal this process
//! ignores, as `nohup` makes it ignore `SIGHUP`, stays ignored.
//!
//! While the viewer has no link, each attempt to get one back runs on its own
//! thread, and the loop keeps reading this terminal. The keys resolve against
//! the base keymap, never the pane placement keymap. The quit keybinding
//! (`<C-q>` by default, in normal and locked mode) leaves at once: the viewer
//! restores its terminal, prints the session it left and the command that
//! reattaches, and exits zero.
//!
//! The quit keybinding never waits for the session's answer. With a link, the
//! viewer queues `core:quit` behind every earlier request, waits until the
//! writer thread has handed the queue to the socket or 1 second has passed,
//! and ends as detached. The session detaches the client when it reads the
//! quit, or when it sees the connection close.
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
use std::ops::ControlFlow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
#[cfg(unix)]
use std::sync::{LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::crossterm::terminal::size;
use ratatui::crossterm::tty::IsTty;

use crate::input::{InputReadClock, KeyOutcome};
use crate::mouse::MouseAction;
use crate::PlacementInputAction;
use crate::{compute_core_pane_area, Client, LinkStatus, PlacementTargetOutcome};
use koshi_config::types::BoundAction;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget,
    LockModeArgs, PanePlacementAnchor, SwitchSessionArgs, TiledPlacementTarget, ToggleLockModeArgs,
    VisualCommand,
};
use koshi_core::constant::{compute_frame_interval, compute_heartbeat_interval};
use koshi_core::geometry::{Direction, PixelCellSize, Rect as CoreRect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{KeyInput, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseInput};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action_with_scroll_line_count, DispatchPlan};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::{IncomingEvent, SessionEvent};
use koshi_ipc::image_chunk::ImageChunkFrame;
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult, WireMouseAction,
};
use koshi_ipc::remote_wire::RemoteSessionRow;
use koshi_ipc::router::{
    resolve_router_endpoint_path, RouterRequestKind, RouterResult, SessionAddress, SessionSelector,
};
use koshi_ipc::transport::{FrameReader, FrameWriter, MAX_FRAME_BYTE_COUNT};
use koshi_ipc::wire::{MaybeKnown, WireName};
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_renderer::placement::PlacementPreviewError;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, FloatingPaneSlot, MouseFrame, PanePlacementMessage,
    PanePlacementPresentation, PlacementPreview, PlacementTabSnapshot, Reconnecting,
    RenderSnapshot, TabSnapshot, ViewerChrome,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::painted_update::{
    PaintedUpdateReceiver, ReceivedPaintedFrame, SessionFrame,
};

use crate::terminal;
use crate::terminal::TerminalInput;
use koshi_core::ids::{format_session_target, parse_owned_session_id, parse_prefixed_uuid};
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
pub(crate) mod tests;

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
/// seconds.
const REDIAL_WINDOW_DURATION: Duration = Duration::from_secs(120);

/// The most keys, fired bindings and pastes a restart rejoin holds for the
/// restarted session: 1024.
const MAX_HELD_INPUT_COUNT: usize = 1024;

/// How long quitting with a link waits for the writer thread to hand every
/// queued request to the socket: 1 second.
const MAX_QUIT_WRITE_WAIT_DURATION: Duration = Duration::from_secs(1);

/// The number the first connection of one attachment carries. Coming back after
/// the session replaces its own process image counts up from here.
const INITIAL_CONNECTION_INDEX: u64 = 0;
/// The wait before the first retry of a native image frame that failed to
/// paint: 1 ms. Each further failure doubles it.
const FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(1);
/// Maximum wakeup after repeated native output failures.
const MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION: Duration = Duration::from_secs(1);

/// Most queued terminal and session events handled before the loop yields to
/// image output, input timers, and outbound requests.
const MAX_INCOMING_EVENT_COUNT_PER_PASS: usize = 16;

/// Most unread terminal and session events retained before their producer waits.
const INCOMING_QUEUE_CAPACITY: usize = MAX_INCOMING_EVENT_COUNT_PER_PASS;

/// Most image bytes copied into the cache during one attachment-loop pass.
const MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH: usize =
    koshi_ipc::frame::MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT;

/// The most decided-but-unwritten mouse actions this client holds, and the most
/// unanswered border moves it remembers: 256. A pile of actions over the cap
/// drops its oldest scrolls, as [`queue_mouse_actions`] says, and the oldest
/// remembered border moves past the cap are forgotten.
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
    /// The input mode that owns the viewer's keyboard
    /// ([`Client::get_active_input_mode`]), whose bindings the hint bar lists.
    pub(crate) lock_mode: LockMode,
    /// Whether the frame's viewer takes the mouse for text selection.
    pub(crate) is_mouse_selection_enabled: bool,
    /// The multi-chord sequence being typed, which the hint bar draws as a
    /// breadcrumb ahead of the chords that continue it. `None` when no sequence
    /// is open.
    pub(crate) pending_key_sequence: Option<KeySequence>,
    /// The checked placement target outlined in the active pane area.
    pub(crate) placement_target: Option<TiledPlacementTarget>,
    /// The whole-pane visual explanation of this viewer's placement.
    pub(crate) placement_presentation: Option<PanePlacementPresentation>,
}

/// How long one placement preview change, committed placement slide, or
/// undock slide runs: 160 ms.
const PLACEMENT_ANIMATION_DURATION: Duration = Duration::from_millis(160);

/// The tab one available placement preview shows with one target placed,
/// built once for that preview and target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementView {
    /// The available preview the view is built from.
    placement_preview: Arc<PlacementPreview>,
    /// The target placed in the preview's displayed tab, or `None` for the tab
    /// as it is.
    placement_target: Option<TiledPlacementTarget>,
    /// The preview's displayed tab with `placement_target` placed
    /// ([`PlacementSnapshot::build_placed_preview`](koshi_renderer::snapshot::PlacementSnapshot::build_placed_preview)),
    /// or as it is with no target or a target the session refuses.
    placed_tab_snapshot: PlacementTabSnapshot,
}

impl PlacementView {
    /// The view of `placement_preview` with the target of
    /// `placement_target_outcome`, the session's answer to a target of that
    /// preview, placed. With no outcome, or an outcome that names a refusal,
    /// the view shows the preview's displayed tab as it is. `None` when the
    /// preview is unavailable or shows no tab.
    fn build(
        placement_preview: Arc<PlacementPreview>,
        placement_target_outcome: Option<&PlacementTargetOutcome>,
    ) -> Option<Self> {
        let placement_snapshot = placement_preview.find_placement_snapshot()?;
        let placed_tab_snapshot =
            match placement_target_outcome.and_then(|placement_target_outcome| {
                placement_target_outcome.placed_preview.as_ref().ok()
            }) {
                Some(placed_preview) => placed_preview.placed_tab_snapshot.clone(),
                None => placement_snapshot.find_displayed_tab_snapshot()?.clone(),
            };
        Some(PlacementView {
            placement_preview,
            placement_target: placement_target_outcome
                .map(|placement_target_outcome| placement_target_outcome.placement_target.clone()),
            placed_tab_snapshot,
        })
    }
}

/// What one paint draws for a placement preview: the preview, and the tab it
/// shows at the paint's time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementDisplay {
    /// The available preview the paint draws from.
    placement_preview: Arc<PlacementPreview>,
    /// The tab the paint draws in place of the active tab.
    displayed_tab_snapshot: PlacementTabSnapshot,
}

/// The viewer-owned interpolation between two tabs a placement preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementAnimation {
    /// The tab drawn when this animation began.
    from_tab_snapshot: PlacementTabSnapshot,
    /// The tab this animation reaches.
    to_tab_snapshot: PlacementTabSnapshot,
    /// The monotonic time at which this interpolation began.
    started_at: Instant,
}

impl PlacementAnimation {
    /// Return the interpolated tab at `current_time`.
    fn build_tab_snapshot(&self, current_time: Instant) -> PlacementTabSnapshot {
        terminal::interpolate_placement_tab_snapshot(
            &self.from_tab_snapshot,
            &self.to_tab_snapshot,
            compute_placement_animation_progress(self.started_at, current_time),
        )
    }
}

/// The viewer-owned slide from the tab this viewer drew to the tab after an
/// accepted pane placement that this viewer's pane placement mode did not
/// send: another viewer's placement, a placement from the command line, or
/// this viewer's undock.
#[derive(Debug)]
struct CommittedPlacementAnimation {
    /// The active tab as drawn when the accepted placement's frame arrived.
    from_tab_snapshot: TabSnapshot,
    /// The monotonic time at which this slide began.
    started_at: Instant,
}

/// The viewer-owned slide of a pane that this viewer drew tiled and a newer
/// frame draws floating, from its tiled box to its floating box.
#[derive(Debug)]
struct UndockAnimation {
    /// The floated pane.
    pane_id: PaneId,
    /// The pane's tiled box as last drawn, counted from the pane-area origin.
    from_outer_rect: CoreRect,
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
    /// `render_snapshot`'s tab list, and names the session's refusal of the
    /// selected placement target ([`Client::find_placement_refusal`]).
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
            placement_target: client.get_placement_target().cloned(),
            placement_presentation: build_pane_placement_presentation(client, render_snapshot),
        }
    }
}

/// The second line of a placement message while the session refuses the
/// selected target.
const PLACEMENT_REFUSAL_DETAIL_TEXT: &str = "Choose another place · Esc to cancel";

/// Describe the proposed positions inside the affected panes. While the
/// session refuses the selected target ([`Client::find_placement_refusal`]),
/// the destination message names that refusal, as
/// [`build_placement_refusal_message`] words it, and the moving pane's message
/// reads `Moving pane cannot go there`.
fn build_pane_placement_presentation(
    client: &Client,
    render_snapshot: &RenderSnapshot,
) -> Option<PanePlacementPresentation> {
    if !client.is_pane_placement_visible() {
        return None;
    }
    let source_pane_id = client.get_placement_source_pane_id()?;
    let destination_tab_name =
        resolve_tab_name(render_snapshot, client.get_placement_destination_tab_id()?);
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
    let is_cross_tab = placement_snapshot
        .find_source_tab_snapshot()
        .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.tab_id)
        != Some(placement_snapshot.destination_tab_id);
    let confirmation_hint = if client.is_placement_confirmation_pending() {
        "Waiting for session"
    } else if client.is_placement_drag_active() {
        "Release to place · Esc to cancel"
    } else {
        "Enter to place · Esc to cancel"
    };
    let (mut target_pane_ids, moving_pane_text, moving_pane_compact_text, target_message) =
        match placement_target {
            TiledPlacementTarget::Swap { target_pane_id } => (
                vec![*target_pane_id],
                "Moving pane will land here".to_string(),
                "Moving pane",
                PanePlacementMessage {
                    full_text: "Other pane will move here".to_string(),
                    compact_text: "Other pane",
                    detail_text: Some("Into the moving pane's old place".to_string()),
                },
            ),
            TiledPlacementTarget::Split {
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
                        let destination_tab_snapshot =
                            placement_snapshot.find_displayed_tab_snapshot();
                        (
                            destination_tab_snapshot
                                .map(|destination_tab_snapshot| {
                                    destination_tab_snapshot
                                        .tab_snapshot
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
            TiledPlacementTarget::Stack { target_pane_id } => (
                vec![*target_pane_id],
                "Moving pane will open in this stack".to_string(),
                "Moving pane",
                PanePlacementMessage {
                    full_text: "Other pane will stack behind it".to_string(),
                    compact_text: "Other pane",
                    detail_text: None,
                },
            ),
        };
    target_pane_ids.retain(|target_pane_id| *target_pane_id != source_pane_id);
    if let Some(placement_refusal) = client.find_placement_refusal() {
        return Some(PanePlacementPresentation {
            source_pane_id,
            target_message: (!target_pane_ids.is_empty()).then(|| {
                build_placement_refusal_message(
                    placement_refusal,
                    placement_snapshot.destination_tab_id,
                    render_snapshot,
                )
            }),
            target_pane_ids,
            source_message: PanePlacementMessage {
                full_text: "Moving pane cannot go there".to_string(),
                compact_text: "Moving pane",
                detail_text: Some(PLACEMENT_REFUSAL_DETAIL_TEXT.to_string()),
            },
        });
    }
    let moving_pane_detail_text = if client.is_placement_confirmation_pending() {
        confirmation_hint.to_string()
    } else if is_cross_tab {
        match placement_target {
            TiledPlacementTarget::Swap { .. } => {
                match placement_snapshot.find_source_tab_snapshot() {
                    Some(source_tab_snapshot) => format!(
                        "Other pane takes the old place in {}",
                        source_tab_snapshot.tab_snapshot.tab_name
                    ),
                    None => format!("In {destination_tab_name}"),
                }
            }
            TiledPlacementTarget::Split { .. } | TiledPlacementTarget::Stack { .. } => {
                format!("In {destination_tab_name}")
            }
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

/// The name of tab `tab_id` in `render_snapshot`'s tab list, or `this tab` for
/// a tab the list does not hold.
fn resolve_tab_name(render_snapshot: &RenderSnapshot, tab_id: TabId) -> &str {
    render_snapshot
        .session_snapshot
        .tabs_metadata
        .iter()
        .find(|tab_metadata| tab_metadata.tab_id == tab_id)
        .map_or("this tab", |tab_metadata| tab_metadata.tab_name.as_str())
}

/// The destination message for the session's refusal `placement_refusal` of a
/// placement into `destination_tab_id`, naming tabs as [`resolve_tab_name`]
/// reads them from `render_snapshot`:
///
/// - [`PlacementPreviewError::NoRoom`] of the destination tab `work` reads
///   `No room for this pane in work`, and of the source tab `work` reads
///   `No room for the panes left in work`.
/// - [`PlacementPreviewError::NoDestinationTabSize`] for the destination tab
///   `work` reads `No room to show work`.
/// - Every other refusal reads `This place is not available`.
///
/// Each carries [`PLACEMENT_REFUSAL_DETAIL_TEXT`] as its second line.
fn build_placement_refusal_message(
    placement_refusal: &PlacementPreviewError,
    destination_tab_id: TabId,
    render_snapshot: &RenderSnapshot,
) -> PanePlacementMessage {
    let (full_text, compact_text) = match placement_refusal {
        PlacementPreviewError::NoRoom { tab_id } if *tab_id == destination_tab_id => (
            format!(
                "No room for this pane in {}",
                resolve_tab_name(render_snapshot, *tab_id)
            ),
            "No room",
        ),
        PlacementPreviewError::NoRoom { tab_id } => (
            format!(
                "No room for the panes left in {}",
                resolve_tab_name(render_snapshot, *tab_id)
            ),
            "No room",
        ),
        PlacementPreviewError::NoDestinationTabSize => (
            format!(
                "No room to show {}",
                resolve_tab_name(render_snapshot, destination_tab_id)
            ),
            "No room",
        ),
        PlacementPreviewError::UnpreviewedTarget | PlacementPreviewError::LayoutRefusal(_) => {
            ("This place is not available".to_string(), "Not available")
        }
    };
    PanePlacementMessage {
        full_text,
        compact_text,
        detail_text: Some(PLACEMENT_REFUSAL_DETAIL_TEXT.to_string()),
    }
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
/// the placement display changes, or while a placement preview, a committed
/// placement or a floated pane slides. Enter or mouse release changes the
/// in-pane message to `Placing this pane…` until the session rejects the
/// placement, or a painted frame names it as the client's last applied
/// placement.
struct Screen<B: Backend> {
    /// The paint driver the renderer paints into.
    paint_driver: terminal::PaintDriver<B>,
    /// The output every frame's bytes pass through on their way to this
    /// terminal. The backend of `paint_driver` writes into the same held
    /// bytes.
    frame_output: terminal::TerminalOutput,
    /// The inputs of the last frame drawn, which the next frame compares its
    /// own against to draw only the rows it changes.
    shown_frame_inputs: Option<terminal::FrameInputs>,
    /// The window title used for the last title-write comparison.
    last_window_title: String,
    /// The cursor style used for the last cursor-style-write comparison.
    last_cursor_style: Option<CursorStyle>,
    /// Whether this viewer requested Shift mouse reports from the outer terminal.
    is_shift_mouse_capture_requested: bool,
    /// The snapshot last drawn. A viewer-only change draws it again without
    /// re-reading the frame. Its grids travel behind `Arc`s, and retaining it
    /// copies no cell data. `None` until the first draw.
    last_snapshot: Option<RenderSnapshot>,
    /// The tab the held placement preview shows with the selected target
    /// placed, or `None` with no available preview.
    placement_view: Option<PlacementView>,
    /// The placement preview and the tab the last successful paint drew for
    /// it, or `None` when that paint drew no preview.
    shown_placement_display: Option<PlacementDisplay>,
    /// The viewer-owned placement interpolation, if one is active.
    placement_animation: Option<PlacementAnimation>,
    /// The active tab drawn by the last successful paint, with any placement
    /// preview or committed-placement slide applied. `None` until the first draw.
    shown_tab_snapshot: Option<TabSnapshot>,
    /// The tabs named by a committed placement ([`CommittedPlacementAnimation`]),
    /// waiting for the next session frame.
    committed_placement_tab_ids: Vec<TabId>,
    /// The slide after a committed placement, if one is active.
    committed_placement_animation: Option<CommittedPlacementAnimation>,
    /// The running slides of floated panes, one per pane, in the order they
    /// started.
    undock_animations: Vec<UndockAnimation>,
    /// The floating pane slots the floated-pane slides drew in the last
    /// successful paint, in slide order. Empty when that paint drew none.
    shown_undock_floating_pane_slots: Vec<FloatingPaneSlot>,
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
    /// `frame_output` takes every byte of each frame other than the ones the
    /// backend of `paint_driver` writes. The image output calls
    /// `notify_image_output_ready` when the next paint has new pixels to show.
    fn with_graphics_support(
        paint_driver: terminal::PaintDriver<B>,
        frame_output: terminal::TerminalOutput,
        viewport_size: Size,
        graphics_support: terminal::GraphicsSupport,
        cell_size: Option<PixelCellSize>,
        notify_image_output_ready: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Screen {
            paint_driver,
            frame_output,
            shown_frame_inputs: None,
            last_window_title: String::new(),
            last_cursor_style: None,
            is_shift_mouse_capture_requested: false,
            last_snapshot: None,
            placement_view: None,
            shown_placement_display: None,
            placement_animation: None,
            shown_tab_snapshot: None,
            committed_placement_tab_ids: Vec::new(),
            committed_placement_animation: None,
            undock_animations: Vec::new(),
            shown_undock_floating_pane_slots: Vec::new(),
            committed_regions: CommittedRegions::build_core(viewport_size, 0),
            shown_viewer_paint: None,
            pending_snapshot: None,
            graphics_support,
            image_output_state: terminal::ImageOutputState::from_output_kind(
                terminal::ImageOutputKind::from_support(graphics_support),
                notify_image_output_ready,
            ),
            native_retry_delay: FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION,
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
    /// surfaces sit, the viewer chrome painted with them, and the per-pane
    /// scroll and mouse fields. That is what the next mouse event is answered
    /// from.
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
    /// and changes nothing. The paint shows no placement preview while
    /// [`Client::is_floating_dock_selection_idle`] is `true`.
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
        let viewport_size = self
            .pending_snapshot
            .as_ref()?
            .client_snapshot
            .viewport_size;
        let committed_regions = self.compute_committed_regions(viewport_size);
        let placement_display = self.update_previewed_placement_display(client, current_time);
        let committed_placement_tab_ids = std::mem::take(&mut self.committed_placement_tab_ids);
        let committed_placement_tab_snapshot = self.update_committed_placement_animation(
            client,
            &committed_placement_tab_ids,
            placement_display.is_some(),
            current_time,
        );
        self.start_undock_animations(current_time);
        let undock_floating_pane_slots =
            self.update_undock_animations(client, placement_display.is_some(), current_time);
        let snapshot = self.pending_snapshot.as_ref()?;
        let displayed_render_snapshot = build_displayed_render_snapshot(
            snapshot,
            placement_display.as_ref(),
            committed_placement_tab_snapshot,
            &undock_floating_pane_slots,
        );
        let displayed_snapshot = displayed_render_snapshot.as_ref().unwrap_or(snapshot);
        let frame_paint = ViewerPaint::from_client(
            client,
            displayed_snapshot.client_snapshot.active_tab_id,
            snapshot,
        );
        match terminal::paint_frame_with_writer(
            &mut self.frame_output,
            &mut self.paint_driver,
            client,
            displayed_snapshot,
            &committed_regions,
            &frame_paint,
            self.graphics_support.get_image_render_mode(),
            &mut self.image_output_state,
            self.cell_size,
            &mut self.shown_frame_inputs,
            &mut self.last_window_title,
            &mut self.last_cursor_style,
            placement_display.as_ref().and_then(|placement_display| {
                placement_display
                    .placement_preview
                    .find_placement_snapshot()
            }),
            placement_display
                .as_ref()
                .map(|placement_display| &placement_display.displayed_tab_snapshot),
        ) {
            Ok(()) => {
                if is_native_image_output {
                    self.native_retry_delay = FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION;
                    self.native_retry_at = None;
                }
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
        let painted_viewer_chrome = frame_paint.chrome;
        self.shown_viewer_paint = Some(frame_paint);
        self.shown_placement_display = placement_display;
        self.shown_undock_floating_pane_slots = undock_floating_pane_slots;
        self.shown_tab_snapshot = Some(
            displayed_snapshot
                .session_snapshot
                .active_tab_snapshot
                .clone(),
        );
        let mouse_frame_snapshot = if self.shown_placement_display.is_some() {
            displayed_snapshot
        } else {
            &snapshot
        };
        let mouse_frame = MouseFrame::from_snapshot(
            mouse_frame_snapshot,
            committed_regions,
            painted_viewer_chrome,
        );
        self.last_snapshot = Some(snapshot);
        Some(mouse_frame)
    }

    /// Commit a pending frame or redraw the last frame when viewer paint or the
    /// placement display changes, or while a placement preview, a committed
    /// placement or a floated pane slides. Enter or mouse release changes the
    /// in-pane message to `Placing this pane…` until the session rejects the
    /// placement, or a painted frame names it as the client's last applied
    /// placement.
    ///
    /// `active_tab_id` is `Some` after the first frame has been drawn. A viewer
    /// with no changed local state, and no finished image request since its
    /// last paint, draws nothing. A viewer whose viewport no longer matches the
    /// committed region solve draws nothing until the next session frame. No
    /// placement preview is drawn while
    /// [`Client::is_floating_dock_selection_idle`] is `true`.
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
        self.last_snapshot.as_ref()?;
        let placement_display = self.update_previewed_placement_display(client, current_time);
        let committed_placement_tab_snapshot = self.update_committed_placement_animation(
            client,
            &[],
            placement_display.is_some(),
            current_time,
        );
        let undock_floating_pane_slots =
            self.update_undock_animations(client, placement_display.is_some(), current_time);
        let snapshot = self.last_snapshot.as_ref()?;
        let displayed_active_tab_id = placement_display.as_ref().map_or(
            snapshot.client_snapshot.active_tab_id,
            |placement_display| placement_display.displayed_tab_snapshot.tab_snapshot.tab_id,
        );
        let viewer_paint = ViewerPaint::from_client(client, displayed_active_tab_id, snapshot);
        let is_committed_tab_shown = placement_display.is_some()
            || self.shown_tab_snapshot.as_ref()
                == Some(
                    committed_placement_tab_snapshot
                        .as_ref()
                        .unwrap_or(&snapshot.session_snapshot.active_tab_snapshot),
                );
        if self.shown_viewer_paint.as_ref() == Some(&viewer_paint)
            && self.shown_placement_display == placement_display
            && is_committed_tab_shown
            && self.shown_undock_floating_pane_slots == undock_floating_pane_slots
            && !self.image_output_state.has_unselected_results()
        {
            return None;
        }
        let displayed_render_snapshot = build_displayed_render_snapshot(
            snapshot,
            placement_display.as_ref(),
            committed_placement_tab_snapshot,
            &undock_floating_pane_slots,
        );
        let displayed_snapshot = displayed_render_snapshot.as_ref().unwrap_or(snapshot);
        if !is_frame_committed(terminal::paint_frame_with_writer(
            &mut self.frame_output,
            &mut self.paint_driver,
            client,
            displayed_snapshot,
            &self.committed_regions,
            &viewer_paint,
            self.graphics_support.get_image_render_mode(),
            &mut self.image_output_state,
            self.cell_size,
            &mut self.shown_frame_inputs,
            &mut self.last_window_title,
            &mut self.last_cursor_style,
            placement_display.as_ref().and_then(|placement_display| {
                placement_display
                    .placement_preview
                    .find_placement_snapshot()
            }),
            placement_display
                .as_ref()
                .map(|placement_display| &placement_display.displayed_tab_snapshot),
        )) {
            return None;
        }
        let painted_viewer_chrome = viewer_paint.chrome;
        self.shown_viewer_paint = Some(viewer_paint);
        self.shown_placement_display = placement_display;
        self.shown_undock_floating_pane_slots = undock_floating_pane_slots;
        self.shown_tab_snapshot = Some(
            displayed_snapshot
                .session_snapshot
                .active_tab_snapshot
                .clone(),
        );
        let mouse_frame_snapshot = if self.shown_placement_display.is_some() {
            displayed_snapshot
        } else {
            snapshot
        };
        Some(MouseFrame::from_snapshot(
            mouse_frame_snapshot,
            self.committed_regions.clone(),
            painted_viewer_chrome,
        ))
    }

    /// [`Self::update_placement_display`] with the selected placement target
    /// placed, or with no target placed while the held left button drives a
    /// floating pane's grab-handle drag
    /// ([`Client::is_floating_pane_move_drag_active`]). The session's answer to
    /// the selected target is built first
    /// ([`Client::update_placement_target_outcome`]). While
    /// [`Client::is_floating_dock_selection_idle`] is `true`, the placement
    /// view and its animation end, and the result is `None`.
    fn update_previewed_placement_display(
        &mut self,
        client: &mut Client,
        current_time: Instant,
    ) -> Option<PlacementDisplay> {
        client.update_placement_target_outcome();
        if client.is_floating_dock_selection_idle() {
            self.placement_view = None;
            self.placement_animation = None;
            return None;
        }
        self.update_placement_display(
            client,
            !client.is_floating_pane_move_drag_active(),
            current_time,
        )
    }

    /// Update the placement view, with the selected target placed when
    /// `is_target_placed` is `true`, and return what a paint at
    /// `current_time` draws for it:
    ///
    /// - With an available held preview
    ///   ([`Client::find_held_placement_preview`]), the view is that preview
    ///   with the target of the held answer
    ///   ([`Client::get_placement_target_outcome`]) placed, or with no target
    ///   placed when `is_target_placed` is `false` or no answer is held
    ///   ([`PlacementView::build`]). It is built again only when the preview or
    ///   the placed target changes.
    /// - With none, the view settles on its preview's tab as it is, and ends
    ///   once the paint shows that tab.
    /// - A view built again starts an animation from the tab drawn now to the
    ///   tab the view shows ([`Self::start_placement_animation`]).
    ///
    /// Returns `None` with no view. While the animation runs, the paint draws
    /// its tab at `current_time`, and after it ends, the view's tab.
    fn update_placement_display(
        &mut self,
        client: &Client,
        is_target_placed: bool,
        current_time: Instant,
    ) -> Option<PlacementDisplay> {
        let held_placement_preview = client
            .find_held_placement_preview()
            .filter(|placement_preview| placement_preview.find_placement_snapshot().is_some());
        let placement_target_outcome = client
            .get_placement_target_outcome()
            .filter(|_| is_target_placed);
        let placement_target = placement_target_outcome
            .map(|placement_target_outcome| &placement_target_outcome.placement_target);
        let is_view_current =
            self.placement_view.as_ref().is_some_and(
                |placement_view| match held_placement_preview {
                    Some(held_placement_preview) => {
                        placement_view.placement_preview == *held_placement_preview
                            && placement_view.placement_target.as_ref() == placement_target
                    }
                    None => placement_view.placement_target.is_none(),
                },
            );
        if !is_view_current {
            let placement_view = match held_placement_preview {
                Some(held_placement_preview) => PlacementView::build(
                    Arc::clone(held_placement_preview),
                    placement_target_outcome,
                ),
                None => self.placement_view.take().and_then(|placement_view| {
                    PlacementView::build(placement_view.placement_preview, None)
                }),
            };
            self.start_placement_animation(
                placement_view.as_ref(),
                client.get_client_config().should_reduce_motion,
                current_time,
            );
            self.placement_view = placement_view;
        }
        let Some(placement_view) = self.placement_view.as_ref() else {
            self.placement_animation = None;
            return None;
        };
        let animated_tab_snapshot = match self.placement_animation.as_ref() {
            Some(placement_animation)
                if compute_placement_animation_progress(
                    placement_animation.started_at,
                    current_time,
                ) < 1.0 =>
            {
                Some(placement_animation.build_tab_snapshot(current_time))
            }
            _ => None,
        };
        if animated_tab_snapshot.is_none() {
            self.placement_animation = None;
        }
        let displayed_tab_snapshot = match animated_tab_snapshot {
            Some(animated_tab_snapshot) => animated_tab_snapshot,
            None if held_placement_preview.is_some() => placement_view.placed_tab_snapshot.clone(),
            None => {
                self.placement_view = None;
                return None;
            }
        };
        Some(PlacementDisplay {
            placement_preview: Arc::clone(&placement_view.placement_preview),
            displayed_tab_snapshot,
        })
    }

    /// Start the animation toward the tab `placement_view` shows, from the tab
    /// drawn now: the running animation's tab at `current_time`, else the tab
    /// the last paint drew for a preview. A drawn tab with another tab id than
    /// the view's tab is replaced by the view preview's tab as it is. No
    /// animation runs with no view, with `should_reduce_motion`, or when the
    /// drawn tab already equals the view's tab.
    ///
    /// A swap target selected in `[A | B]` slides `A` and `B` from their slots
    /// to each other's slots.
    fn start_placement_animation(
        &mut self,
        placement_view: Option<&PlacementView>,
        should_reduce_motion: bool,
        current_time: Instant,
    ) {
        let Some(placement_view) = placement_view.filter(|_| !should_reduce_motion) else {
            self.placement_animation = None;
            return;
        };
        let to_tab_snapshot = &placement_view.placed_tab_snapshot;
        let drawn_tab_snapshot = match self.placement_animation.as_ref() {
            Some(placement_animation) => Some(placement_animation.build_tab_snapshot(current_time)),
            None => self
                .shown_placement_display
                .as_ref()
                .map(|placement_display| placement_display.displayed_tab_snapshot.clone()),
        }
        .filter(|drawn_tab_snapshot| {
            drawn_tab_snapshot.tab_snapshot.tab_id == to_tab_snapshot.tab_snapshot.tab_id
        });
        let from_tab_snapshot = drawn_tab_snapshot.or_else(|| {
            placement_view
                .placement_preview
                .find_placement_snapshot()
                .and_then(|placement_snapshot| placement_snapshot.find_displayed_tab_snapshot())
                .cloned()
        });
        self.placement_animation = from_tab_snapshot
            .filter(|from_tab_snapshot| from_tab_snapshot != to_tab_snapshot)
            .map(|from_tab_snapshot| PlacementAnimation {
                from_tab_snapshot,
                to_tab_snapshot: to_tab_snapshot.clone(),
                started_at: current_time,
            });
    }

    /// Start or advance the slide after a committed placement
    /// ([`CommittedPlacementAnimation`]), and return the active tab it draws at
    /// `current_time`.
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

    /// Start one slide at `current_time` for each pane that the pending frame
    /// draws floating and the tab this screen drew last shows tiled with
    /// content. A pane whose slide runs already keeps it. Each slide runs from
    /// the pane's tiled box, counted from the pane-area origin of the last
    /// paint ([`terminal::find_tiled_pane_rect_in_pane_area`]).
    ///
    /// A screen that drew `A | B | C` and a pending frame that draws `B` and
    /// `C` floating start two slides. A pane that leaves the tab and is not
    /// floating in the pending frame starts none.
    fn start_undock_animations(&mut self, current_time: Instant) {
        let (Some(pending_snapshot), Some(shown_tab_snapshot)) = (
            self.pending_snapshot.as_ref(),
            self.shown_tab_snapshot.as_ref(),
        ) else {
            return;
        };
        let pane_area_size = self.committed_regions.solved_regions.pane_rect.size;
        for floating_pane_slot in &pending_snapshot.session_snapshot.floating_pane_slots {
            let pane_id = floating_pane_slot.pane_id;
            let is_sliding = self
                .undock_animations
                .iter()
                .any(|undock_animation| undock_animation.pane_id == pane_id);
            if is_sliding {
                continue;
            }
            if let Some(from_outer_rect) = terminal::find_tiled_pane_rect_in_pane_area(
                shown_tab_snapshot,
                pane_area_size,
                pane_id,
            ) {
                self.undock_animations.push(UndockAnimation {
                    pane_id,
                    from_outer_rect,
                    started_at: current_time,
                });
            }
        }
    }

    /// Advance every floated-pane slide and return the floating pane slots
    /// they draw at `current_time`, in slide order.
    ///
    /// The frame is the pending frame, else the last drawn frame. Every slide
    /// ends, and the result is empty, when `is_placement_shown` is `true` or
    /// reduced motion is on. A slide ends once `PLACEMENT_ANIMATION_DURATION`
    /// has passed, or when the frame draws its pane in no floating slot. Each
    /// other slide draws its pane between its tiled box and the frame's
    /// floating box.
    ///
    /// A pane drawn at `(0, 0)` `40x22` that floats to `(16, 4)` `48x13` draws
    /// at `(8, 2)` `44x17` at progress `0.5`.
    fn update_undock_animations(
        &mut self,
        client: &Client,
        is_placement_shown: bool,
        current_time: Instant,
    ) -> Vec<FloatingPaneSlot> {
        let Some(snapshot) = self
            .pending_snapshot
            .as_ref()
            .or(self.last_snapshot.as_ref())
        else {
            return Vec::new();
        };
        if is_placement_shown || client.get_client_config().should_reduce_motion {
            self.undock_animations.clear();
            return Vec::new();
        }
        let floating_pane_slots = &snapshot.session_snapshot.floating_pane_slots;
        let mut animated_floating_pane_slots = Vec::new();
        self.undock_animations.retain(|undock_animation| {
            let progress =
                compute_placement_animation_progress(undock_animation.started_at, current_time);
            let floating_pane_slot = floating_pane_slots
                .iter()
                .find(|floating_pane_slot| floating_pane_slot.pane_id == undock_animation.pane_id);
            let floating_pane_slot = match floating_pane_slot {
                Some(floating_pane_slot) if progress < 1.0 => floating_pane_slot,
                _ => return false,
            };
            animated_floating_pane_slots.push(FloatingPaneSlot::from_outer_rect(
                floating_pane_slot.pane_id,
                terminal::interpolate_rect(
                    undock_animation.from_outer_rect,
                    floating_pane_slot.outer_rect,
                    progress,
                ),
                floating_pane_slot.is_pinned,
            ));
            true
        });
        animated_floating_pane_slots
    }

    /// Record the tabs a committed placement ([`CommittedPlacementAnimation`])
    /// changed. The next session frame whose active tab is one of `tab_ids` slides from the
    /// tab this screen drew. A `None` names the floating side of a placement
    /// and records no tab.
    fn note_committed_placement(&mut self, tab_ids: [Option<TabId>; 2]) {
        self.committed_placement_tab_ids
            .extend(tab_ids.into_iter().flatten());
    }

    /// Return the wait before the retry of a native image frame that failed
    /// to paint, or `None` when no such frame waits. New image pixels wake
    /// the loop through [`Incoming::ImageOutputReady`], not through this
    /// wakeup.
    fn compute_next_image_wakeup_at(&self, current_time: Instant) -> Option<Duration> {
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
    /// interpolation, committed-placement slide or floated-pane slide: the shorter of
    /// `frame_interval` and the time the animation has left. `None` when no
    /// animation runs.
    fn compute_next_placement_animation_wakeup_at(
        &self,
        current_time: Instant,
        frame_interval: Duration,
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
            .chain(
                self.undock_animations
                    .iter()
                    .map(|undock_animation| undock_animation.started_at),
            )
            .min()?;
        let animation_remaining = PLACEMENT_ANIMATION_DURATION
            .saturating_sub(current_time.saturating_duration_since(animation_started_at));
        Some(animation_remaining.min(frame_interval))
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
            self.native_retry_delay = FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION;
            self.native_retry_at = None;
            if self.pending_snapshot.is_none() {
                self.pending_snapshot.clone_from(&self.last_snapshot);
            }
        }
    }

    /// Record that the client's image records hold `held_image_byte_count`
    /// bytes of decoded pixels. Cached encoded output past the room they
    /// leave in the image budget is dropped.
    fn set_held_image_byte_count(&mut self, held_image_byte_count: u64) {
        self.image_output_state
            .set_held_image_byte_count(held_image_byte_count);
    }

    /// Drop the cached encoded output that reads an image in
    /// `image_content_ids`, which the session released.
    fn release_image_contents(&mut self, image_content_ids: &[u64]) {
        self.image_output_state
            .release_image_contents(image_content_ids);
    }

    /// Reset native output state after the session image cache or connection changes.
    fn reset_connection(&mut self) {
        self.image_output_state.reset_connection();
        self.native_retry_delay = FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION;
        self.native_retry_at = None;
        self.placement_animation = None;
        self.shown_placement_display = None;
        self.committed_placement_tab_ids.clear();
        self.committed_placement_animation = None;
        self.undock_animations.clear();
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

/// Return the frame painted in place of `snapshot`:
///
/// - with `placement_display`, `snapshot` showing that display's tab
///   ([`terminal::build_placement_render_snapshot`]);
/// - else with `committed_placement_tab_snapshot` or a non-empty
///   `undock_floating_pane_slots`, `snapshot` with that slide frame, if any,
///   as its active tab, and each of `undock_floating_pane_slots` in place of
///   the floating pane slot that names the same pane;
/// - else `None`, and `snapshot` is painted as it is.
fn build_displayed_render_snapshot(
    snapshot: &RenderSnapshot,
    placement_display: Option<&PlacementDisplay>,
    committed_placement_tab_snapshot: Option<TabSnapshot>,
    undock_floating_pane_slots: &[FloatingPaneSlot],
) -> Option<RenderSnapshot> {
    if let Some(placement_display) = placement_display {
        let (source_pane_id, _) = placement_display.placement_preview.get_placement_target();
        return Some(terminal::build_placement_render_snapshot(
            snapshot,
            &placement_display.displayed_tab_snapshot,
            source_pane_id,
        ));
    }
    if committed_placement_tab_snapshot.is_none() && undock_floating_pane_slots.is_empty() {
        return None;
    }
    Some(terminal::build_committed_placement_render_snapshot(
        snapshot,
        committed_placement_tab_snapshot,
        undock_floating_pane_slots,
    ))
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
        terminal::PaintError::Output(output_error) => {
            tracing::warn!(%output_error, "could not write the frame to the terminal")
        }
    }
}

/// Report a paint error and return whether the complete frame was committed.
fn is_frame_committed<BackendError: std::fmt::Debug>(
    paint_result: Result<(), terminal::PaintError<BackendError>>,
) -> bool {
    match paint_result {
        Ok(()) => true,
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
    /// The connection broke: the session server is gone, or a link to a
    /// session on another machine read nothing for `remote-link-timeout-ms`.
    ConnectionDied,
    /// This terminal went away, or this process took a termination signal,
    /// while the session kept running.
    TerminalGone,
    /// The session moved this client to the session named here.
    SwitchSession {
        /// The session to attach to next.
        session_id: SessionId,
        /// The user id whose folder of the shared directory advertises
        /// `session_id`, or `None` for a session looked up by its id alone.
        owner_user_id: Option<u32>,
    },
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
    /// A remote viewer's link broke and its redial gave up, carrying the cause
    /// it gave up on: the refusal a dial got, the last unreachable cause when
    /// the redial window closed, or the panic of an attempt's thread. The
    /// session keeps running without this viewer.
    LinkLost(Box<CliError>),
    /// The quit keybinding fired while the viewer had no link: during a redial
    /// or while a restarted session was being joined. The session keeps
    /// running without this viewer.
    LeftWhileReconnecting,
}

/// Two endings are equal when they are the same variant carrying the same
/// fields. A [`AttachmentEnding::SwitchSession`] compares its [`SessionId`] and
/// its user id. A [`AttachmentEnding::LinkLost`] and a
/// [`AttachmentEnding::RefusedAfterRestart`] compare the text their cause
/// prints, which is what the viewer shows.
impl PartialEq for AttachmentEnding {
    fn eq(&self, other_ending: &Self) -> bool {
        match (self, other_ending) {
            (AttachmentEnding::Detached, AttachmentEnding::Detached)
            | (AttachmentEnding::SessionEnded, AttachmentEnding::SessionEnded)
            | (AttachmentEnding::ConnectionDied, AttachmentEnding::ConnectionDied)
            | (AttachmentEnding::TerminalGone, AttachmentEnding::TerminalGone)
            | (AttachmentEnding::Restarting, AttachmentEnding::Restarting)
            | (AttachmentEnding::LeftWhileReconnecting, AttachmentEnding::LeftWhileReconnecting)
            | (
                AttachmentEnding::RestartedIntoIncompatibleBuild,
                AttachmentEnding::RestartedIntoIncompatibleBuild,
            ) => true,
            (
                AttachmentEnding::SwitchSession {
                    session_id: first_session_id,
                    owner_user_id: first_owner_user_id,
                },
                AttachmentEnding::SwitchSession {
                    session_id: second_session_id,
                    owner_user_id: second_owner_user_id,
                },
            ) => {
                first_session_id == second_session_id && first_owner_user_id == second_owner_user_id
            }
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
/// an event read from this terminal, or the answer of an attempt to get the
/// link back.
///
/// All three arrive on one channel, so one blocking read serves the session,
/// the keyboard and the attempt at once.
enum Incoming {
    /// A frame the session wrote, or the read that failed.
    Frame {
        /// Which connection the reader that sent this was reading. A client
        /// that came back after the session replaced its own process image
        /// reads a subsequent connection: connection 0 is the first, 1 the one after
        /// the first restart, and so on.
        connection_index: u64,
        /// The frame itself, or the read that failed. A painted update that
        /// does not decode or apply is a failed read.
        session_event_result: Result<SessionEvent, IpcError>,
    },
    /// The image output has new pixels for the next paint: the worker ended
    /// a request, or a paint left finished Kitty uploads unwritten. The pass
    /// that takes it ends with [`Screen::refresh_at`], which paints them. The
    /// sender drops this event when the channel is full; the events that fill
    /// the channel each end a pass the same way.
    ImageOutputReady,
    /// The last part of a painted update the session wrote, applied by the
    /// connection's reader.
    Painted {
        /// Which connection the reader that sent this was reading, as
        /// [`Incoming::Frame`] numbers it.
        connection_index: u64,
        /// The applied update.
        received_painted_frame: Box<ReceivedPaintedFrame>,
    },
    /// One chunk of an image record the session wrote, in the frame the
    /// connection's reader read it in.
    ImageChunk {
        /// Which connection the reader that sent this was reading, as
        /// [`Incoming::Frame`] numbers it.
        connection_index: u64,
        /// The chunk.
        image_chunk_frame: ImageChunkFrame,
    },
    /// The events of one read of this terminal and the instant it was read,
    /// or this terminal hanging up as a termination signal or a console close
    /// reports it, boxed.
    Input(Box<TerminalInput>),
    /// What one attempt to get the link back ended with, boxed. Only
    /// [`run_without_link`] starts an attempt, one at a time.
    RejoinAnswer(Box<RejoinAnswer>),
}

/// What one attempt to get the link back ended with, as its
/// `koshi-attach-rejoin` thread puts it on the loop's channel.
enum RejoinAnswer {
    /// The attempt joined the session on a new connection, held until the
    /// loop takes it.
    Joined(JoinedSessionGuard),
    /// The attempt did not reach the session, for the cause carried here. A
    /// redial dials again if its next pause fits the window.
    Unreachable(CliError),
    /// The attempt ended the attachment with the ending carried here.
    Ended(AttachmentEnding),
}

/// A connection that joined the session, held until the viewer takes it with
/// [`release_joined_session`](Self::release_joined_session).
///
/// Dropped before the viewer takes it, it sends
/// `Leaving { is_returning: true }` on its connection, then closes the
/// connection: the session keeps running without this viewer. Example: a quit
/// read before a rejoin attempt's join ends the loop, and the join still on
/// the loop's channel says `Leaving` when the channel is dropped. A join whose
/// rejoin attempt finds the loop's channel gone, and the first join of a
/// terminal that cannot be set up, say `Leaving` the same way.
struct JoinedSessionGuard {
    /// The joined connection. `None` once the viewer took it.
    joined_session: Option<JoinedSession>,
}

impl JoinedSessionGuard {
    /// Hold `joined_session` until the viewer takes it.
    fn from_joined_session(joined_session: JoinedSession) -> Self {
        JoinedSessionGuard {
            joined_session: Some(joined_session),
        }
    }

    /// Hand the joined connection to the viewer. Nothing is sent on it.
    fn release_joined_session(mut self) -> JoinedSession {
        self.joined_session
            .take()
            .expect("the guard holds its connection until it is released")
    }
}

impl Drop for JoinedSessionGuard {
    /// Send `Leaving { is_returning: true }`, under the request id
    /// [`FIRST_POST_ATTACH_REQUEST_ID`], on a connection that the viewer did
    /// not take. A connection that the viewer took sends nothing here. A failed
    /// write is logged.
    fn drop(&mut self) {
        let Some(mut joined_session) = self.joined_session.take() else {
            return;
        };
        let leaving_request = IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Leaving { is_returning: true },
        };
        if let Err(write_error) = joined_session.writer.send(&leaving_request) {
            tracing::warn!(
                %write_error,
                "could not tell the session that this viewer left a join it did not take"
            );
        }
    }
}

/// This client's side of the connection: the writer thread that takes its
/// requests, the action table a fired binding is resolved against, the
/// number the next request carries, and the placement target the link names.
struct Uplink {
    /// The writer thread every request the loop sends goes to, or `None` once
    /// [`drain_request_queue`](Self::drain_request_queue) closed its queue.
    uplink_writer: Option<UplinkWriter>,
    /// The action table a fired binding is turned into commands with. The
    /// session owns its own table and runs those commands; this one only names
    /// what they are.
    registry: ActionRegistry,
    /// The `request_id` the next request carries.
    next_request_id: u64,
    /// The placement target this link last named as the client's pane
    /// placement preview, in the link's Attach or in its last
    /// `SetPanePlacementPreview`, or `None` when it names none.
    sent_placement_preview_target: Option<PanePlacementPreviewTarget>,
}

/// One writer thread [`spawn_uplink_writer`] started: the queue it writes from,
/// and the signal of its end.
struct UplinkWriter {
    /// The queue the thread writes from. Dropping it closes the queue, and the
    /// thread ends once it has written every request already on it.
    request_sender: mpsc::Sender<IpcRequest>,
    /// Reports [`mpsc::RecvTimeoutError::Disconnected`] once the thread has
    /// ended. Nothing is ever sent on it.
    writer_end_receiver: mpsc::Receiver<()>,
}

impl Uplink {
    /// Queue one request for the writer thread, numbering it, and give back the
    /// `request_id` it carried. The queue takes it whatever the socket is
    /// doing, so this never waits.
    ///
    /// A request over the frame cap is dropped by the writer and never
    /// answered. A queue nobody takes from is a writer thread that has ended,
    /// which only a broken connection does; the reading half meets that same
    /// connection and reports it. With the queue closed, the request is
    /// dropped and the number is still taken.
    fn send_request(&mut self, request_kind: IpcRequestKind) -> u64 {
        let request_id = self.next_request_id;
        let ipc_request = IpcRequest {
            request_id,
            request_kind,
        };
        self.next_request_id += 1;
        if let Some(uplink_writer) = &self.uplink_writer {
            let _ = uplink_writer.request_sender.send(ipc_request);
        }
        request_id
    }

    /// Close the request queue and wait until its writer thread has written
    /// every queued request and ended, or until `deadline`. Returns whether
    /// the queue drained: `true` when the thread ended by `deadline`, and
    /// `true` at once when no queue is open. A `deadline` already passed waits
    /// for nothing.
    ///
    /// A writer thread still writing at `deadline` runs on with the queue it
    /// holds; the next request this client sends goes nowhere.
    fn drain_request_queue(&mut self, deadline: Instant) -> bool {
        let Some(UplinkWriter {
            request_sender,
            writer_end_receiver,
        }) = self.uplink_writer.take()
        else {
            return true;
        };
        drop(request_sender);
        let wait_duration = deadline.saturating_duration_since(Instant::now());
        matches!(
            writer_end_receiver.recv_timeout(wait_duration),
            Err(mpsc::RecvTimeoutError::Disconnected)
        )
    }

    /// Name `placement_target` as the client's pane placement preview
    /// (`SetPanePlacementPreview`) when it differs from the target this link
    /// named last. Sends nothing when they are equal.
    fn send_placement_preview_target(
        &mut self,
        placement_target: Option<PanePlacementPreviewTarget>,
    ) {
        if self.sent_placement_preview_target == placement_target {
            return;
        }
        self.send_request(IpcRequestKind::SetPanePlacementPreview { placement_target });
        self.sent_placement_preview_target = placement_target;
    }

    /// Queue `command` under `command_id`, issued by `client`'s key binding.
    fn submit_key_binding_command(
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

    /// Send what one viewer-local action, or one mouse event that pane
    /// placement owns, asks of the session:
    ///
    /// - `FocusPane` sends a mouse `FocusPane` for its pane.
    /// - `SubmitCommand` sends its command under its `command_id`, issued by
    ///   `client`'s key binding.
    /// - `ConfirmPlacement` sends one `ConfirmPanePlacement`.
    /// - `Consumed` sends nothing.
    fn submit_placement_input_action(
        &mut self,
        client: &Client,
        placement_input_action: PlacementInputAction,
    ) {
        match placement_input_action {
            PlacementInputAction::FocusPane { pane_id } => {
                self.submit_mouse_focus_pane(client, pane_id);
            }
            PlacementInputAction::SubmitCommand {
                command_id,
                command,
            } => {
                self.submit_key_binding_command(client, command_id, command);
            }
            PlacementInputAction::ConfirmPlacement {
                command_id,
                source_pane_id,
                placement_target,
                expected_placed_tab_layout,
            } => {
                self.send_request(IpcRequestKind::ConfirmPanePlacement {
                    command_id,
                    source_pane_id,
                    placement_target,
                    expected_placed_tab_layout: *expected_placed_tab_layout,
                });
            }
            PlacementInputAction::Consumed => {}
        }
    }

    /// Resolve `bound_action` against the action table with `client`'s own
    /// new-pane direction and scroll line count. `None` when the table refuses
    /// the action.
    ///
    /// The new-pane direction is this viewer's own setting, so a pane-opening
    /// binding that names no direction already says where the pane goes by the
    /// time the command leaves.
    fn resolve_bound_action(
        &self,
        client: &Client,
        bound_action: &BoundAction,
    ) -> Option<DispatchPlan> {
        resolve_action_with_scroll_line_count(
            &bound_action.action_reference,
            &self.registry,
            client.get_client_config().layout.new_pane_direction,
            client.get_client_config().mouse.scroll_line_count,
        )
        .ok()
    }

    /// Resolve one fired binding against the action table and run its plan
    /// against `mouse_frame`, the frame this terminal last painted. Returns
    /// [`ControlFlow::Break`] when the plan is [`Command::Quit`], which is
    /// queued like every other session command, and [`ControlFlow::Continue`]
    /// for every other binding.
    ///
    /// A lock binding switches `client` to the lock mode it sets at once, as
    /// [`Client::apply_lock_command`] says, and goes up as `SetLockMode` for
    /// that mode under a new command id: `core:toggle-lock` in `Normal` goes
    /// up as `SetLockMode` with `is_locked: true`. Every other plan runs as
    /// [`Self::run_dispatch_plan`] runs it. An action the table refuses sends
    /// nothing.
    fn submit_bound_action(
        &mut self,
        client: &mut Client,
        bound_action: BoundAction,
        mouse_frame: Option<&MouseFrame>,
    ) -> ControlFlow<()> {
        let Some(dispatch_plan) = self.resolve_bound_action(client, &bound_action) else {
            return ControlFlow::Continue(());
        };
        if let Some(lock_mode) = find_lock_mode_set_by(&dispatch_plan, client.get_lock_mode()) {
            let lock_command_id = CommandId::new();
            client.apply_lock_command(lock_command_id, lock_mode);
            self.submit_key_binding_command(
                client,
                lock_command_id,
                build_set_lock_mode_command(lock_mode),
            );
            return ControlFlow::Continue(());
        }
        let is_quit_plan = is_quit_dispatch_plan(&dispatch_plan);
        self.run_dispatch_plan(client, dispatch_plan, mouse_frame);
        if is_quit_plan {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    /// Run `dispatch_plan`: queue its command under a new command id, issued by
    /// `client`'s key binding, or run its viewer-local action on `client`
    /// against `mouse_frame`, the frame this terminal last painted, and send
    /// what that action asks of the session.
    fn run_dispatch_plan(
        &mut self,
        client: &mut Client,
        dispatch_plan: DispatchPlan,
        mouse_frame: Option<&MouseFrame>,
    ) {
        match dispatch_plan {
            DispatchPlan::Command(command) => {
                self.submit_key_binding_command(client, CommandId::new(), *command);
            }
            DispatchPlan::ClientAction(client_action_kind) => {
                let placement_input_action =
                    client.apply_client_action(client_action_kind, mouse_frame);
                self.submit_placement_input_action(client, placement_input_action);
            }
        }
    }
}

/// Whether `dispatch_plan` runs [`Command::Quit`].
fn is_quit_dispatch_plan(dispatch_plan: &DispatchPlan) -> bool {
    matches!(dispatch_plan, DispatchPlan::Command(command) if **command == Command::Quit)
}

/// The lock mode `dispatch_plan` sets on the client that runs it while that
/// client is in `current_lock_mode`, or `None` when the plan is no lock
/// command for that client. `core:lock` gives `Locked` and `core:unlock` gives
/// `Normal`. `core:toggle-lock` gives `Normal` in `Locked` and `Locked` in
/// every other mode. A lock command that names a client gives `None`.
fn find_lock_mode_set_by(
    dispatch_plan: &DispatchPlan,
    current_lock_mode: LockMode,
) -> Option<LockMode> {
    let DispatchPlan::Command(command) = dispatch_plan else {
        return None;
    };
    match **command {
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }) => Some(LockMode::Locked),
        Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id: None,
        }) => Some(LockMode::Normal),
        Command::ToggleLockMode(ToggleLockModeArgs { client_id: None }) => {
            match current_lock_mode {
                LockMode::Locked => Some(LockMode::Normal),
                _ => Some(LockMode::Locked),
            }
        }
        _ => None,
    }
}

/// The `SetLockMode` command that puts the client that runs it in
/// `lock_mode`: `is_locked` is `true` for `Locked` and `false` for every
/// other mode, and no client is named.
fn build_set_lock_mode_command(lock_mode: LockMode) -> Command {
    Command::SetLockMode(LockModeArgs {
        is_locked: lock_mode == LockMode::Locked,
        client_id: None,
    })
}

/// Where the session this client joins runs.
///
/// One home is picked before the first connection opens. A restart re-enters
/// the session through the same home. A switch keeps the home's machine, and on
/// this machine it takes the user id the switch names.
#[derive(Clone)]
enum Home {
    /// A session on this machine, joined through the endpoint file it
    /// advertises under `runtime_directory`, or through the socket a folder of
    /// the shared directory advertises for it.
    Local {
        /// This user's runtime directory, holding one endpoint file per
        /// session.
        runtime_directory: PathBuf,
        /// The id of another user whose folder of the shared directory
        /// advertises the session: every dial reads that one folder. `None`
        /// looks the session's id up in `runtime_directory`, then in the shared
        /// directory.
        owner_user_id: Option<u32>,
    },
    /// A session on another machine, joined through the server serving it.
    Remote {
        /// The saved record every dial presents: the address, the secret, and
        /// the fingerprint the server's certificate must have.
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

/// Resolve what the user typed to one running session, through
/// [`resolve_local_session_target`].
///
/// `selector` is a `session-<uuid>` id, a bare UUID, a session display name,
/// or `<user id>/<session id>`. `None` picks one from the sessions running for
/// this user and the sessions `shared_sessions_base_directory` advertises
/// instead: nothing running is a failure, one session is taken straight away,
/// and more than one is printed as a numbered list to answer on stdin.
fn resolve_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    selector: Option<&str>,
) -> Result<(SessionId, Option<u32>), CliError> {
    let selector = match selector {
        Some(selector) => selector.to_string(),
        // No remote rows are offered, so every place is a local one.
        None => match select_session(
            runtime_directory,
            shared_sessions_base_directory,
            Vec::new(),
        )? {
            SessionSelection::Local(selector) => selector,
            SessionSelection::Remote(remote_row_index) => {
                unreachable!(
                    "a listing offered no remote rows and settled on place {remote_row_index}"
                )
            }
        },
    };
    resolve_local_session_target(runtime_directory, shared_sessions_base_directory, &selector)
}

/// The running session `selector` names on this machine, and the user id whose
/// folder of the shared directory advertises it.
///
/// A `selector` such as `1001/session-<uuid>` is read by
/// [`parse_owned_session_id`], and
/// [`load_session_endpoint`](ipc_client::load_session_endpoint) finds that
/// session in the folder of user `1001` alone: the answer carries `Some(1001)`.
/// Every other `selector` is a `session-<uuid>` id, a bare UUID, or a session
/// display name that [`lookup_session_address`] asks the router for: the answer
/// carries `None`.
///
/// # Errors
/// [`CliError::InvalidArgs`] for a `selector` with a `/` that is not
/// `<user id>/session-<uuid>`. Every error of
/// [`load_session_endpoint`](ipc_client::load_session_endpoint) and of
/// [`lookup_session_address`].
fn resolve_local_session_target(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    selector: &str,
) -> Result<(SessionId, Option<u32>), CliError> {
    let owned_session_id =
        parse_owned_session_id(selector).map_err(|detail| CliError::InvalidArgs { detail })?;
    match owned_session_id {
        Some(owned_session_id) => {
            ipc_client::load_session_endpoint(
                runtime_directory,
                shared_sessions_base_directory,
                owned_session_id.session_id,
                Some(owned_session_id.owner_user_id),
            )?;
            Ok((
                owned_session_id.session_id,
                Some(owned_session_id.owner_user_id),
            ))
        }
        None => Ok((
            lookup_session_address(runtime_directory, selector)?.session_id,
            None,
        )),
    }
}

/// Join a running session in this terminal as a new client.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, a session display name,
/// or `<user id>/<session id>`: `1001/session-<uuid>` names that session in the
/// folder of user `1001` of the shared directory. `None` picks one from the
/// sessions running for this user and the sessions on every saved server that
/// answered: no session running anywhere is a failure, exactly one session on
/// this machine is taken straight away, and anything else — several sessions,
/// or one session on a saved server — is printed as a numbered list to answer
/// on stdin.
pub fn attach_selected_session(selector: Option<&str>) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let config_directory = koshi_paths::resolve_config_directory();
    let Some(selector) = selector else {
        return attach_selected_session_from_listing(
            runtime_directory,
            config_directory.as_deref(),
        );
    };
    let (session_id, owner_user_id) = resolve_local_session_target(
        &runtime_directory,
        koshi_link::config::find_shared_sessions_base_directory(config_directory.as_deref())
            .as_deref(),
        selector,
    )?;
    attach_home(
        Home::Local {
            runtime_directory,
            owner_user_id,
        },
        SessionSelector::SessionId(session_id),
        config_directory.as_deref(),
    )
}

/// Join a session on the machine `server` names in this terminal.
///
/// `server` is either the name this machine saved that server under or the
/// `host:port` it listens on. `certificate_fingerprint` is the fingerprint the
/// certificate of the server must have: a server reached for the first time
/// needs it, and a saved server takes the one it holds and refuses any other.
/// `save_as` is the name to save the server under: a server reached for the
/// first time, or a saved server that holds no name, takes it; a saved server
/// that holds that name already takes it too; a saved server that holds
/// another name refuses it.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display name,
/// and the server resolves it against the sessions this machine's secret
/// reaches. `None` lists those sessions instead: one is taken straight away,
/// and more than one is printed as a numbered list to answer on stdin. Nothing
/// here creates a session.
///
/// # Errors
/// [`CliError::Runtime`] when the server does not admit the secret, and when
/// nothing this secret reaches is running on it. [`CliError::IpcUnavailable`]
/// when the certificate it presents is not the expected one.
/// [`CliError::InvalidArgs`] when a server reached for the first time comes
/// with no fingerprint or a malformed one, when a saved server is given a
/// fingerprint other than the one it holds, when it holds none, when it
/// holds a name other than `save_as`, and when `KOSHI_REMOTE_SECRET` holds a
/// secret no saved server holds.
pub fn attach_remote_session(
    server: &str,
    certificate_fingerprint: Option<&str>,
    save_as: Option<&str>,
    selector: Option<&str>,
) -> Result<(), CliError> {
    let server_reference = remote_client::resolve_server(server, certificate_fingerprint)?;
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
        Home::Remote {
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
/// list, such as a server that refused or did not answer. A picked remote row
/// is joined through the saved server that listed it, which presents the
/// secret it holds. `KOSHI_REMOTE_SECRET` is not read.
///
/// `config_directory` holds the `koshi.kdl` that names the shared directory
/// other users' sessions are listed from, and the config the attachment reads.
fn attach_selected_session_from_listing(
    runtime_directory: PathBuf,
    config_directory: Option<&Path>,
) -> Result<(), CliError> {
    let reachable_session_rows = remote_client::list_saved_server_session_rows();
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
        SessionSelection::Local(selector) => {
            let (session_id, owner_user_id) = resolve_local_session_target(
                &runtime_directory,
                shared_sessions_base_directory.as_deref(),
                &selector,
            )?;
            return attach_home(
                Home::Local {
                    runtime_directory,
                    owner_user_id,
                },
                SessionSelector::SessionId(session_id),
                config_directory,
            );
        }
        SessionSelection::Remote(remote_session_index) => {
            &reachable_session_rows[remote_session_index]
        }
    };
    attach_home(
        Home::Remote {
            server: remote_client::resolve_listed_server(server_label)?,
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
/// UUID, a session display name, or `<user id>/<session id>`, and `None` picks
/// one from the sessions running for this user. A session on another machine
/// is never offered. The session moves the client this terminal already holds.
pub fn switch_in_session(
    session_context: &InSessionContext,
    selector: Option<&str>,
) -> Result<CommandResult, CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let shared_sessions_base_directory = ipc_client::resolve_shared_sessions_base_directory();
    let (session_id, owner_user_id) = resolve_session(
        &runtime_directory,
        shared_sessions_base_directory.as_deref(),
        selector,
    )?;
    ipc_client::submit_in_session_command(
        session_context,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id,
            owner_user_id,
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
        None,
    )?;
    attach_home(
        Home::Local {
            runtime_directory: runtime_directory.to_path_buf(),
            owner_user_id: None,
        },
        SessionSelector::SessionId(session_id),
        config_directory,
    )
}

/// Join the session `session_selector` names in `home` and run until nothing moves this
/// client on.
///
/// A session that moves this client to another one is attached to next, on the
/// same machine. A client on this machine dials the next session with the user
/// id the move names, as [`Home::Local`] states. A client on a server dials
/// that server again with the next session's id alone, so the certificate, the
/// secret and the scope are all checked again before the next session paints
/// anything.
///
/// The path this program was started from, as
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// reads it, is read once, here, before the first attachment. `None` when the
/// platform cannot report it. `config_directory` is handed to every
/// attachment.
fn attach_home(
    mut home: Home,
    session_selector: SessionSelector,
    config_directory: Option<&Path>,
) -> Result<(), CliError> {
    let client_executable_path = koshi_host::program_path::resolve_program_path().ok();
    let mut next_session_selector = session_selector;
    loop {
        let attach_result = attach_once(
            &home,
            &next_session_selector,
            client_executable_path.as_deref(),
            config_directory,
        );
        end_process_after_termination_signal();
        let Some((next_session_id, next_owner_user_id)) = attach_result? else {
            return Ok(());
        };
        next_session_selector = SessionSelector::SessionId(next_session_id);
        if let Home::Local { owner_user_id, .. } = &mut home {
            *owner_user_id = next_owner_user_id;
        }
    }
}

/// Join the session `session_selector` names in `home` and run one attachment of it,
/// handing back the session to attach to next, and the user id whose shared
/// folder advertises it, when this one moved the client on.
///
/// The terminal enters raw mode for its capability probe, then enters the
/// alternate screen after Attach succeeds. This call owns both states and
/// leaves them before it returns, so the terminal is restored between one
/// session and the next. A terminal that cannot be set up after the session
/// accepted the client returns the error, and the connection sends `Leaving`
/// with `is_returning` `true` before it closes: the session keeps running.
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
) -> Result<Option<(SessionId, Option<u32>)>, CliError> {
    let (loaded_config, config_warnings) = koshi_link::config::load_config_files(config_directory);
    let client_config =
        koshi_link::config::resolve_client_config(loaded_config.app_config_layer.clone());
    let mut terminal_owner =
        terminal::TerminalOwner::open_terminal_owner(client_config.supports_image_protocols)
            .map_err(|detail| CliError::Runtime { detail })?;
    let graphics_support = terminal_owner.get_graphics_support();
    let attach_settings = AttachSettings {
        graphics_support,
        max_frames_per_second: client_config.max_frames_per_second,
    };
    let mut cell_size_query = terminal_owner.build_cell_size_query();
    let joined_session = dial_session(
        home,
        session_selector,
        attach_settings,
        cell_size_query.get_current_cell_size(),
        config_directory,
        client_config.remote_link_timeout_ms,
    )?;
    let client_id = joined_session.client_id;
    // Until the terminal is set up, the guard holds the join: a return before
    // that sends `Leaving` with `is_returning` `true` and closes the connection.
    let joined_session_guard = JoinedSessionGuard::from_joined_session(joined_session);

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
    // A terminal that closes, or a termination signal, ends the attachment as
    // `TerminalGone`, whose last request says this viewer does not return.
    // With no watch, either one ends the process at once.
    let terminal_close_watch =
        match TerminalCloseWatch::start_terminal_close_watch(incoming_sender.clone()) {
            Ok(terminal_close_watch) => terminal_close_watch,
            Err(watch_error) => {
                tracing::warn!(%watch_error, "could not watch for this terminal closing");
                None
            }
        };
    let should_read_input = io::stdin().is_tty();
    let input_read_clock = Arc::new(InputReadClock::default());
    terminal_owner
        .activate_terminal(
            input_sender,
            client_id,
            Arc::clone(&input_read_clock),
            should_read_input,
        )
        .map_err(|activation_detail| CliError::Runtime {
            detail: activation_detail,
        })?;
    let JoinedSession {
        reader,
        writer,
        client_id: _,
        session_id,
        connection_token,
        resume_token,
    } = joined_session_guard.release_joined_session();
    if should_read_input {
        spawn_input_relay(input_receiver, incoming_sender.clone());
    } else {
        drop(input_receiver);
        tracing::info!("standard input is not a terminal, so this client reads no keys");
    }

    // One channel, four producers: the reading half of every connection this
    // attachment holds in turn, this terminal's input thread, each attempt to
    // get a link back, and the loop itself, which keeps a sender to start the
    // reader and the attempts it comes back on. A broken connection reaches
    // the loop as the failed read its own reader writes here, so the link ends
    // on that frame, not on the channel closing.
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
    let mut client = terminal::build_client_with_loaded_config(
        client_id,
        get_terminal_viewport_size(),
        terminal_cleanup_guard,
        loaded_config,
        input_read_clock,
    );
    client.set_session_id(session_id);
    let mut uplink = Uplink {
        uplink_writer: Some(spawn_uplink_writer(writer)),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
        sent_placement_preview_target: None,
    };
    // Every frame is drawn at the client's viewport size, read here and on
    // each resize event. A terminal that reports no size, which is what a
    // `koshi attach` with redirected output finds, draws at
    // [`FALLBACK_VIEWPORT`], the size this client told the session it has.
    let frame_output = terminal::TerminalOutput::from_terminal_sink(Box::new(io::stdout()));
    let paint_driver = terminal::PaintDriver::from_backend(
        terminal::TerminalOutputBackend::from_terminal_output(&frame_output),
        client.get_viewport_size(),
    );
    let image_output_ready_sender = incoming_sender.clone();
    let mut screen = Screen::with_graphics_support(
        paint_driver,
        frame_output,
        client.get_viewport_size(),
        graphics_support,
        cell_size_query.get_current_cell_size(),
        Arc::new(move || {
            let _ = image_output_ready_sender.try_send(Incoming::ImageOutputReady);
        }),
    );

    let mut terminal_writer = io::stdout();
    let ending = run_attachment(
        home,
        session_id,
        client_id,
        connection_token,
        resume_token,
        &mut client,
        &mut screen,
        &mut uplink,
        attach_settings,
        &mut cell_size_query,
        &mut terminal_writer,
        incoming_sender,
        incoming_receiver,
    );

    // The terminal is restored before the ending message is printed, and the
    // message lands on the shell's own screen. The screen holds no unwritten
    // byte: each frame ends with a flush of its output. Shutting the terminal
    // owner down writes the mode cleanup, which leaves the alternate screen
    // and shows the cursor. Dropping the client then runs its cleanup guard.
    // The watch stops after that, and a termination signal it took ends the
    // process before anything is printed.
    drop(screen);
    terminal_owner.shutdown_terminal();
    drop(client);
    drop(terminal_close_watch);
    end_process_after_termination_signal();
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
/// A link the loop leaves to get another one gets `Leaving` as its last
/// request, and its writer thread ends once it has written it. Then
/// [`run_without_link`] runs: a session replacing its own process image comes
/// back through [`reconnect_after_restart`], and a remote viewer whose link
/// breaks dials its server again through [`dial_remote`] while
/// `remote-reconnect` is on. Every event left in the batch that ended the link
/// goes to [`run_without_link`] first, in order. A link it gets back is
/// installed, the terminal's size is reported on it, and the input a restart
/// held is sent after that. A local viewer whose link breaks ends the loop.
///
/// The quit keybinding ends the loop at once. With a link, `core:quit` goes up
/// first, and the loop waits up to [`MAX_QUIT_WRITE_WAIT_DURATION`] for the
/// writer thread to hand it to the socket, then ends as
/// [`AttachmentEnding::Detached`] without waiting for the session's `Detached`
/// frame. With no link, the loop ends as
/// [`AttachmentEnding::LeftWhileReconnecting`].
///
/// Each input event is judged at the instant the input thread read it. Every
/// input timer due at that instant runs before the event: an open sequence's
/// ambiguity deadline, a placement tab hover, and a selection drag's edge
/// scroll ([`expire_input_timers`]). At the end of a pass the timers run at the
/// clock, and they do not run while an event the input thread read has not
/// reached the loop. Example: the input thread reads `a` 400 ms into
/// `<C-y>`'s 500 ms deadline, and the loop draws a frame until 600 ms: `a`
/// continues the sequence.
///
/// On `Home::Remote`, the loop sends `Ping` in its first pass, after the first
/// group of events already queued, at most
/// [`MAX_INCOMING_EVENT_COUNT_PER_PASS`], then once every heartbeat interval of
/// `remote-link-timeout-ms` ([`compute_heartbeat_interval`]) after the last
/// one, on the link it holds then. A `Ping` that falls due while the loop
/// holds no link goes up on the link it gets back, after that link's size
/// report. Example: `30000` sends one at the start, then one every 10 seconds.
///
/// A loop that ends as [`AttachmentEnding::TerminalGone`] or
/// [`AttachmentEnding::SwitchSession`] queues `Leaving` with `is_returning`
/// set to `false` as its last request, then waits up to
/// [`MAX_QUIT_WRITE_WAIT_DURATION`] for the writer thread to hand it to the
/// socket.
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
/// on the next redial to get that attach's view back, and on the return after
/// a restart to prove the claim on `client_id`. Every reconnection stamps it
/// with the secret its own attach minted.
///
/// `terminal_writer` takes the bytes this client writes to its terminal
/// outside a painted frame: each Shift mouse reporting change, and each host
/// write. A host write is the bytes of every `HostWritePart` read on the
/// current connection since its last `HostWrite`, then that `HostWrite`'s
/// bytes, in one `write_all`. A new connection drops the parts held from the
/// connection before it. Example: the parts `ab` and `cd`, then `HostWrite`
/// `ef`, write `abcdef` once.
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
    attach_settings: AttachSettings,
    cell_size_query: &mut terminal::CellSizeQuery,
    terminal_writer: &mut W,
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
    // The bytes of every `HostWritePart` read on the current connection since
    // its last `HostWrite`. The next `HostWrite` writes them before its own
    // bytes, in one write. A new connection starts with none.
    let mut held_host_write_bytes: Vec<u8> = Vec::new();
    // A remote home sends a `Ping` at once, then every third of the link
    // timeout on whichever link the loop holds; a local home sends none.
    let remote_link_timeout_ms = client.get_client_config().remote_link_timeout_ms;
    let ping_interval = compute_heartbeat_interval(remote_link_timeout_ms);
    // A placement preview animation and every slide step once per frame
    // interval.
    let frame_interval = compute_frame_interval(client.get_client_config().max_frames_per_second);
    let mut next_ping_at = match home {
        Home::Remote { .. } => Some(Instant::now()),
        Home::Local { .. } => None,
    };

    let attachment_ending = loop {
        let current_time = Instant::now();
        let next_wakeup_duration = select_earliest_duration(
            select_earliest_duration(
                select_earliest_duration(
                    client.compute_next_key_wakeup(current_time),
                    client.compute_next_mouse_wakeup(current_time),
                ),
                next_ping_at.map(|ping_due_at| ping_due_at.saturating_duration_since(current_time)),
            ),
            select_earliest_duration(
                client.compute_next_placement_tab_hover_wakeup(current_time),
                select_earliest_duration(
                    screen.compute_next_image_wakeup_at(current_time),
                    screen.compute_next_placement_animation_wakeup_at(current_time, frame_interval),
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
        // image output, input timers, and outbound requests run between groups.
        let (incoming_batch_events, deferred_event) =
            build_incoming_batch(incoming_event, &incoming_receiver);
        deferred_incoming_event = deferred_event;

        let mut incoming_batch_events = incoming_batch_events.into_iter().peekable();
        let mut attachment_ending = None;
        // The last painted update this pass applied.
        let mut applied_update_sequence = None;
        while let Some(incoming_event) = incoming_batch_events.next() {
            match incoming_event {
                // A frame read from a connection this client has already left:
                // the reader of the connection before a restart ends by
                // reporting that socket closing.
                Incoming::Frame {
                    connection_index, ..
                }
                | Incoming::Painted {
                    connection_index, ..
                }
                | Incoming::ImageChunk {
                    connection_index, ..
                } if connection_index != current_connection_index => {}
                Incoming::Painted {
                    received_painted_frame,
                    ..
                } => {
                    let update_sequence = received_painted_frame.update_sequence;
                    let adopted_render_snapshot =
                        match image_cache.adopt_painted_frame(received_painted_frame) {
                            Ok(adopted_render_snapshot) => adopted_render_snapshot,
                            Err(painted_frame_error) => {
                                tracing::warn!(
                                    %painted_frame_error,
                                    "could not accept a painted frame"
                                );
                                attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                break;
                            }
                        };
                    applied_update_sequence = Some(update_sequence);
                    // An update that the next event of the group replaces with
                    // a newer update is applied and not drawn.
                    let is_replaced_by_next_update = matches!(
                        incoming_batch_events.peek(),
                        Some(Incoming::Painted { connection_index, .. })
                            if *connection_index == current_connection_index
                    );
                    if let Some(render_snapshot) = adopted_render_snapshot {
                        if !is_replaced_by_next_update {
                            draw_completed_painted_frame(
                                screen,
                                client,
                                cell_size_query,
                                &mut last_mouse_frame,
                                render_snapshot,
                            );
                        }
                    }
                }
                Incoming::ImageChunk {
                    image_chunk_frame, ..
                } => match image_cache.accept_image_chunk(&image_chunk_frame) {
                    Ok(Some(render_snapshot)) => {
                        draw_completed_painted_frame(
                            screen,
                            client,
                            cell_size_query,
                            &mut last_mouse_frame,
                            render_snapshot,
                        );
                    }
                    Ok(None) => {}
                    Err(chunk_error) => {
                        tracing::warn!(%chunk_error, "could not accept an image chunk");
                        attachment_ending = Some(AttachmentEnding::ConnectionDied);
                        break;
                    }
                },
                Incoming::Frame {
                    session_event_result,
                    ..
                } => {
                    if let Some(frame_ending) = classify_session_event(&session_event_result) {
                        attachment_ending = Some(frame_ending);
                        break;
                    }
                    match session_event_result {
                        Ok(SessionEvent::ImageCacheReset) => {
                            image_cache.clear_image_cache();
                            screen.reset_connection();
                        }
                        Ok(SessionEvent::ImageContentReleased { image_content_ids }) => {
                            if let Err(release_error) =
                                image_cache.release_image_contents(&image_content_ids)
                            {
                                tracing::warn!(%release_error, "could not release image records");
                                attachment_ending = Some(AttachmentEnding::ConnectionDied);
                                break;
                            }
                            screen.release_image_contents(&image_content_ids);
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
                        Ok(SessionEvent::PanePlacementCommitted {
                            command_id,
                            source_tab_id,
                            destination_tab_id,
                            ..
                        }) => {
                            if !client.is_pending_placement_command(command_id) {
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
                            client.forget_sent_floating_pane_move();
                        }
                        // The first bytes of a long host write wait for the
                        // `HostWrite` that ends it.
                        Ok(SessionEvent::HostWritePart { host_write_bytes }) => {
                            held_host_write_bytes.extend_from_slice(&host_write_bytes);
                        }
                        // Bytes a pane aimed at this terminal, such as an OSC 52
                        // clipboard write, go to it verbatim: the held parts,
                        // then these bytes, in one write.
                        Ok(SessionEvent::HostWrite { host_write_bytes }) => {
                            held_host_write_bytes.extend_from_slice(&host_write_bytes);
                            let _ = terminal_writer.write_all(&held_host_write_bytes);
                            let _ = terminal_writer.flush();
                            held_host_write_bytes = Vec::new();
                        }
                        // Every other frame is a `Pong`, which carries nothing,
                        // or reports a change to the session's structure, which
                        // the next painted frame carries whole. Only a painted
                        // frame is drawn.
                        _ => {}
                    }
                }
                // An input thread runs only for a terminal that had keys to
                // read, so its hangup is this terminal going away while the
                // session runs on.
                Incoming::Input(terminal_input)
                    if matches!(
                        terminal_input.runtime_events.as_slice(),
                        [RuntimeEvent::Quit]
                    ) =>
                {
                    attachment_ending = Some(AttachmentEnding::TerminalGone);
                    break;
                }
                Incoming::Input(terminal_input) => {
                    if handle_terminal_input_event(
                        client,
                        uplink,
                        cell_size_query,
                        last_mouse_frame.as_ref(),
                        *terminal_input,
                        &mut sent_border_moves,
                        &mut pending_mouse_actions,
                    )
                    .is_break()
                    {
                        attachment_ending = Some(leave_after_quit(uplink));
                        break;
                    }
                }
                // No answer reaches this loop: [`run_without_link`] reads the
                // answer of its one running attempt before it hands a link
                // back, and the loop ends when it hands none back.
                Incoming::RejoinAnswer(_) => {}
                // The pass ends with the refresh that paints the result.
                Incoming::ImageOutputReady => {}
            }
            screen.set_held_image_byte_count(image_cache.compute_held_image_byte_count());
        }
        if let Some(attachment_ending) = attachment_ending {
            let placement_preview_target = client.find_placement_preview_target();
            let (link_recovery, attempt_rejoin): (
                LinkRecovery,
                Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
            ) = match (attachment_ending, home) {
                // The session is replacing its own process image. The terminal
                // keeps every mode it is in and the screen is left alone; the
                // session's first frame on the new connection paints the panes
                // again.
                (AttachmentEnding::Restarting, _) => {
                    let rejoin_home = home.clone();
                    let rejoin_connection_token = connection_token.clone();
                    let rejoin_resume_token = resume_token.clone();
                    let cell_size = cell_size_query.get_current_cell_size();
                    (
                        LinkRecovery::RestartRejoin {
                            input_hold: Some(InputHold::default()),
                        },
                        Arc::new(move || {
                            match reconnect_after_restart(
                                &rejoin_home,
                                session_id,
                                client_id,
                                &rejoin_connection_token,
                                rejoin_resume_token.as_ref(),
                                attach_settings,
                                cell_size,
                                placement_preview_target,
                                remote_link_timeout_ms,
                            ) {
                                Ok(joined_session) => RejoinAnswer::Joined(
                                    JoinedSessionGuard::from_joined_session(joined_session),
                                ),
                                Err(restart_ending) => RejoinAnswer::Ended(restart_ending),
                            }
                        }),
                    )
                }
                // The link to a session on a server broke, and
                // `remote-reconnect` is on: the viewer dials that server again.
                (AttachmentEnding::ConnectionDied, Home::Remote { server })
                    if client.get_client_config().should_reconnect_remote_session =>
                {
                    let redial_server = server.clone();
                    let redial_resume_token = resume_token.clone();
                    let cell_size = cell_size_query.get_current_cell_size();
                    (
                        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
                        Arc::new(move || {
                            build_redial_answer(dial_remote(
                                &redial_server,
                                &SessionSelector::SessionId(session_id),
                                None,
                                redial_resume_token.as_ref(),
                                attach_settings,
                                cell_size,
                                placement_preview_target,
                                remote_link_timeout_ms,
                            ))
                        }),
                    )
                }
                (attachment_ending, _) => break attachment_ending,
            };
            // The last request on this connection. The queue is written in
            // order, so it leaves behind every key already on it, and the
            // writer thread ends once it has written it. This viewer attaches
            // again.
            uplink.send_request(IpcRequestKind::Leaving { is_returning: true });
            let _ = uplink.drain_request_queue(Instant::now());
            client.prepare_placement_reconciliation();
            let leftover_events: Vec<Incoming> = incoming_batch_events
                .chain(deferred_incoming_event.take())
                .collect();
            let (joined_session, held_inputs) = match run_without_link(
                link_recovery,
                leftover_events,
                &attempt_rejoin,
                client,
                screen,
                last_mouse_frame
                    .as_ref()
                    .map(|mouse_frame| mouse_frame.client_snapshot.active_tab_id),
                uplink,
                cell_size_query,
                &incoming_sender,
                &incoming_receiver,
            ) {
                Ok(rejoined_link) => rejoined_link,
                Err(no_link_ending) => break no_link_ending,
            };
            if joined_session.client_id != client_id {
                tracing::warn!(
                    %session_id,
                    "the session did not hand this client back; this viewer goes on as the new client it minted"
                );
            }
            client_id = joined_session.client_id;
            connection_token = joined_session.connection_token;
            resume_token = joined_session.resume_token;
            client.set_client_id(client_id);
            client.end_mouse_gestures();
            pending_mouse_actions.clear();
            // The panes the old frame placed may be gone. Mouse events wait for
            // the new connection's first frame to be placed against.
            last_mouse_frame = None;
            current_connection_index += 1;
            image_cache.clear_image_cache();
            held_host_write_bytes = Vec::new();
            spawn_frame_reader(
                joined_session.reader,
                current_connection_index,
                incoming_sender.clone(),
            );
            uplink.uplink_writer = Some(spawn_uplink_writer(joined_session.writer));
            uplink.next_request_id = FIRST_POST_ATTACH_REQUEST_ID;
            uplink.sent_placement_preview_target = placement_preview_target;
            let locally_measured_cell_size = terminal::read_local_cell_size();
            report_terminal_size_with_cell_size(
                client,
                uplink,
                cell_size_query,
                locally_measured_cell_size,
            );
            send_held_inputs(client, uplink, held_inputs);
            // The new connection numbers its rounds from the start, so no
            // answer to a border move written on the old one can arrive. The
            // next move asks for its whole distance from the drag anchor.
            sent_border_moves.clear();
            screen.reset_connection();
            screen.set_held_image_byte_count(image_cache.compute_held_image_byte_count());
            if let Err(io_error) = screen.update_shift_mouse_capture(client, terminal_writer) {
                tracing::warn!(%io_error, "could not update Shift mouse reporting");
                break AttachmentEnding::TerminalGone;
            }
            continue;
        }
        let current_time = Instant::now();
        if next_ping_at.is_some_and(|ping_due_at| current_time >= ping_due_at) {
            uplink.send_request(IpcRequestKind::Ping);
            next_ping_at = Some(current_time + ping_interval);
        }
        // The input timers run at the clock only once every input event the
        // input thread read has reached this loop.
        if let Some(input_judge_time) = client.get_input_read_clock().find_input_judge_time() {
            if expire_input_timers(
                client,
                uplink,
                last_mouse_frame.as_ref(),
                &mut sent_border_moves,
                &mut pending_mouse_actions,
                input_judge_time,
            )
            .is_break()
            {
                break leave_after_quit(uplink);
            }
        }
        uplink.send_placement_preview_target(client.find_placement_preview_target());
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
            adopt_painted_mouse_frame(client, &mut last_mouse_frame, mouse_frame);
        }
        if let Err(io_error) = screen.update_shift_mouse_capture(client, terminal_writer) {
            tracing::warn!(%io_error, "could not update Shift mouse reporting");
            break AttachmentEnding::TerminalGone;
        }
        // A capture that a key or a painted frame ended outside a mouse event
        // owes its program a release, which joins this pass's round.
        queue_mouse_actions(
            &mut pending_mouse_actions,
            client.take_mouse_capture_release().into_iter().collect(),
        );
        flush_mouse_round(uplink, &mut sent_border_moves, &mut pending_mouse_actions);
        // The last request of the pass reports the last painted update it
        // applied.
        if let Some(update_sequence) = applied_update_sequence {
            uplink.send_request(IpcRequestKind::PaintedUpdate { update_sequence });
        }
    };
    // A viewer whose terminal went away, or that the session moved to another
    // session, does not attach again: its last request says so. A link this
    // loop already left takes nothing.
    if matches!(
        attachment_ending,
        AttachmentEnding::TerminalGone | AttachmentEnding::SwitchSession { .. }
    ) {
        uplink.send_request(IpcRequestKind::Leaving {
            is_returning: false,
        });
        if !uplink.drain_request_queue(Instant::now() + MAX_QUIT_WRITE_WAIT_DURATION) {
            tracing::warn!("the connection did not take the leave in time; leaving without it");
        }
    }
    attachment_ending
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

/// Return the RGBA byte cost of one queued image chunk: its chunk byte
/// count. Every other event costs `0`.
fn compute_incoming_image_byte_count(incoming_event: &Incoming) -> usize {
    match incoming_event {
        Incoming::ImageChunk {
            image_chunk_frame, ..
        } => image_chunk_frame.get_chunk_bytes().len(),
        Incoming::Frame { .. }
        | Incoming::Painted { .. }
        | Incoming::Input(_)
        | Incoming::RejoinAnswer(_)
        | Incoming::ImageOutputReady => 0,
    }
}

/// Open one connection into the session `session_selector` names in `home` and join it as
/// a client.
///
/// On this machine the session's endpoint file names the socket and holds the
/// token the Hello presents, and a display name is resolved by the router
/// first. A session with no endpoint file in the runtime directory is looked
/// up in the shared directory `koshi.kdl` in `config_directory` names, read
/// again on every dial. A home that names another user's id reads only that
/// user's folder of the shared directory. A session of this user's that
/// refuses this build's protocol version is dialed again once it has
/// restarted, and one that refuses the connection token is dialed again once
/// it advertises another, as
/// [`run_session_exchange_with_restart_wait`](ipc_client::run_session_exchange_with_restart_wait)
/// states. One that refuses the join while it replaces its process image, or
/// whose connection ends while its resume file shows it doing so, is dialed
/// again over the endpoint its new image advertises, as
/// [`run_session_exchange_across_restart`](ipc_client::run_session_exchange_across_restart)
/// states. On a server the whole admission runs — TLS with the check of the
/// certificate against the fingerprint the saved record holds, the secret, and
/// the scope check on the session asked for — and the server resolves the name
/// against the sessions that secret reaches. A server whose router answers
/// that it is restarting, or whose session refuses the join while it replaces
/// its process image, is dialed again, as
/// [`dial_through_router_restart`](remote_client::dial_through_router_restart)
/// states, for up to [`RESTART_WINDOW_DURATION`].
fn dial_session(
    home: &Home,
    session_selector: &SessionSelector,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    config_directory: Option<&Path>,
    remote_link_timeout_ms: u32,
) -> Result<JoinedSession, CliError> {
    match home {
        Home::Local {
            runtime_directory,
            owner_user_id,
        } => {
            // The router turns a display name into a session's address before
            // this terminal joins it, and every dial after the first names the
            // session by id.
            let session_id = match session_selector {
                SessionSelector::SessionId(session_id) => *session_id,
                SessionSelector::SessionName(session_name) => {
                    lookup_session_address(runtime_directory, session_name)?.session_id
                }
            };
            ipc_client::run_session_exchange_across_restart(
                runtime_directory,
                koshi_link::config::find_shared_sessions_base_directory(config_directory)
                    .as_deref(),
                session_id,
                *owner_user_id,
                None,
                |endpoint| {
                    let (mut reader, mut writer) =
                        ipc_client::connect_to_session(endpoint, session_id, None)?.split();
                    let (client_id, session_id, resume_token) = join_session(
                        &mut reader,
                        &mut writer,
                        &endpoint.connection_token,
                        None,
                        None,
                        attach_settings,
                        cell_size,
                        None,
                        None,
                    )?;
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
                    attach_settings,
                    cell_size,
                    None,
                    remote_link_timeout_ms,
                )
            })
            .map_err(CliError::from)
        }
    }
}

/// Dial `server`, ask it for the session `session_selector` names, and join that session
/// as a client: open the connection, then [`join_remote_session`] over it. The
/// join runs under a deadline of [`remote_client::JOIN_TIMEOUT_DURATION`] after
/// the connection opens until the session answers `Prepared`. The `Commit` and
/// its answer then run under a deadline of the same length after the `Commit`
/// starts to be written.
///
/// # Errors
/// [`DialError::Unreachable`] when the connection could not be opened.
/// [`DialError::Refused`] when the saved record holds no certificate
/// fingerprint, the certificate the server presents is not the expected one,
/// the server does not admit the secret, or the remote protocol versions do not
/// overlap. Every error that [`join_remote_session`] returns.
#[allow(clippy::too_many_arguments)]
fn dial_remote(
    server: &ServerReference,
    session_selector: &SessionSelector,
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
    remote_link_timeout_ms: u32,
) -> Result<JoinedSession, DialError> {
    let (remote_link, saved_server) = remote_client::connect_saved_server(
        server,
        None,
        Some(remote_client::JOIN_TIMEOUT_DURATION),
    )?;
    join_remote_session(
        remote_link,
        saved_server.connection_token,
        session_selector,
        resume_client_id,
        resume_token,
        attach_settings,
        cell_size,
        placement_preview_target,
        remote_link_timeout_ms,
    )
}

/// Join the session `session_selector` names, as a client, over `remote_link`:
/// an open connection to a server that admitted `connection_token`. The
/// returned session carries `connection_token`.
///
/// The serving machine presents that session's endpoint token and writes the
/// Hello on this client's behalf, so the first frame read back is the session
/// server's answer to that Hello. The Attach after it is this client's own,
/// sent inside a `Prepare` numbered `2` and run by a `Commit` numbered `3`.
/// `resume_client_id` names the client record to come back as, and
/// `resume_token` is the secret the last attach minted, presented to get that
/// attach's view back. `placement_preview_target` is the placement target the
/// client previews, and is `None` on a first join.
///
/// The join runs under the deadline that `remote_link` holds until the
/// session answers `Prepared`. The `Commit` and its answer then run under a
/// deadline [`remote_client::JOIN_TIMEOUT_DURATION`] after the `Commit` starts
/// to be written. `remote_link_timeout_ms` goes up in the serving machine's
/// Attach and in the session's Attach. Until the session's answer to the
/// `Prepare`, then to the `Commit`, is read, a `Ping` numbered `2` goes up
/// every heartbeat interval of `remote_link_timeout_ms`
/// ([`compute_heartbeat_interval`]). Example: `3000` and a session that
/// answers the `Prepare` after 2.5 seconds send two `Ping` requests, at 1 and
/// 2 seconds. Once joined, the returned reader and
/// writer hold no deadline. Each socket read of the reader waits at most
/// `remote_link_timeout_ms` for the session's next byte, and each socket
/// write of the writer waits at most `remote_link_timeout_ms` for the link to
/// take a byte
/// ([`FrameReader::set_idle_timeout`], [`FrameWriter::set_idle_timeout`]).
/// A read or a write that waits longer fails with a timeout.
///
/// # Errors
/// [`DialError::Unreachable`] when a frame of the join could not be written or
/// read. After the `Commit` starts to be written, it carries
/// [`CliError::CommittedRequestUnanswered`]: the session may still attach the
/// client. [`DialError::Restarting`] when the serving router refused the join
/// with [`koshi_ipc::router::ROUTER_RESTARTING_MESSAGE`], and carrying
/// [`CliError::SessionRestarting`] when the session refused the `Prepare` or
/// the `Commit` while it replaces its process image. [`DialError::Refused`]
/// when the server answered and every identical join after it gets the same
/// answer: the admitted secret does not reach `session_selector`, the session
/// protocol versions do not overlap, or its answer is a frame this attach
/// cannot read. An answer to the `Commit` of another kind carries
/// [`CliError::CommittedRequestUnanswered`].
#[allow(clippy::too_many_arguments)]
fn join_remote_session(
    remote_link: remote_client::RemoteLink,
    connection_token: ConnectionToken,
    session_selector: &SessionSelector,
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
    remote_link_timeout_ms: u32,
) -> Result<JoinedSession, DialError> {
    let link_timeout = Duration::from_millis(u64::from(remote_link_timeout_ms));
    let (mut reader, mut writer) =
        remote_client::attach_remote_session(remote_link, session_selector.clone(), link_timeout)
            .map_err(DialError::Unreachable)?;
    let hello_response = remote_client::read_forwarded_hello_answer(&mut reader, session_selector)?;
    talk::parse_session_hello_answer(hello_response).map_err(DialError::Refused)?;
    writer
        .send(&build_attach_prepare_request(
            resume_client_id,
            resume_token,
            attach_settings,
            cell_size,
            Some(remote_link_timeout_ms),
            placement_preview_target,
        ))
        .map_err(build_link_failure)?;
    let ping_interval = compute_heartbeat_interval(remote_link_timeout_ms);
    let prepare_response = read_answer_while_pinging(&mut reader, &mut writer, ping_interval)
        .map_err(build_link_failure)?;
    validate_attach_prepare_answer(prepare_response)?;
    let commit_deadline = Instant::now() + remote_client::JOIN_TIMEOUT_DURATION;
    reader.set_deadline(Some(commit_deadline));
    writer.set_deadline(Some(commit_deadline));
    let commit_request = IpcRequest {
        request_id: 3,
        request_kind: IpcRequestKind::Commit,
    };
    writer
        .send(&commit_request)
        .map_err(build_committed_attach_failure)?;
    let attach_response = read_answer_while_pinging(&mut reader, &mut writer, ping_interval)
        .map_err(build_committed_attach_failure)?;
    let attach_result = match attach_response.answer_result {
        MaybeKnown::Known(attach_result) => attach_result,
        MaybeKnown::Unknown { variant_name } => {
            return Err(DialError::Refused(CliError::CommittedRequestUnanswered {
                detail: talk::SESSION_PEER_WORDS.format_unexpected_reply_detail(&variant_name),
            }))
        }
    };
    let (client_id, session_id, minted_resume_token) =
        parse_attached_session(attach_result).map_err(DialError::from_session_refusal)?;

    // Joined: the join's deadline is taken away, and each socket read and
    // write waits at most the link timeout for a byte.
    reader.set_deadline(None);
    writer.set_deadline(None);
    reader.set_idle_timeout(Some(link_timeout));
    writer.set_idle_timeout(Some(link_timeout));
    Ok(JoinedSession {
        reader,
        writer,
        client_id,
        session_id,
        connection_token,
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

/// The [`DialError::Unreachable`] for `ipc_error`, a failed write of this
/// client's `Commit` or a failed read of its answer: it carries what
/// [`talk::build_committed_request_unanswered_error`] gives. The session may
/// still attach the client it holds.
fn build_committed_attach_failure(ipc_error: IpcError) -> DialError {
    DialError::Unreachable(talk::build_committed_request_unanswered_error(ipc_error))
}

/// Read the next answer on `reader`, and write a `Ping`, numbered 2, on
/// `writer` every `ping_interval` until the answer is read. A `Ping` that
/// cannot be written ends the pings, and the read goes on.
///
/// Example: an interval of 1 second and an answer read after 2.5 seconds
/// write two `Ping` requests, at 1 and 2 seconds.
///
/// # Errors
/// The failure of the read.
fn read_answer_while_pinging(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    ping_interval: Duration,
) -> Result<IncomingResponse, IpcError> {
    thread::scope(|join_scope| {
        // Dropped once the answer is read, which ends the pings.
        let (answer_read_sender, answer_read_receiver) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("koshi-join-pinger".to_string())
            .spawn_scoped(join_scope, move || {
                while answer_read_receiver.recv_timeout(ping_interval)
                    == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    let join_ping = IpcRequest {
                        request_id: 2,
                        request_kind: IpcRequestKind::Ping,
                    };
                    if writer.send(&join_ping).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn join ping thread");
        let incoming_response = reader.recv_answer();
        drop(answer_read_sender);
        incoming_response
    })
}

/// `Ok` when `prepare_response` is the session's `Prepared` answer to the
/// `Prepare` carrying this client's Attach.
///
/// # Errors
/// What [`DialError::from_session_refusal`] gives for what
/// [`talk::build_peer_refusal_error`] gives for a refusal, and
/// [`DialError::Refused`] carrying the unexpected reply for every other answer.
fn validate_attach_prepare_answer(prepare_response: IncomingResponse) -> Result<(), DialError> {
    let prepare_result = talk::SESSION_PEER_WORDS
        .take_response_result(prepare_response)
        .map_err(DialError::Refused)?;
    match prepare_result {
        IpcResult::Prepared => Ok(()),
        IpcResult::Error(refusal) => Err(DialError::from_session_refusal(
            talk::build_peer_refusal_error(&refusal),
        )),
        unexpected_result => Err(DialError::Refused(
            talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result),
        )),
    }
}

/// Come back into `session_id` after it said it is replacing its own process
/// image, and hand back the connection this viewer came back on.
///
/// On this machine [`rejoin_session`] waits for the session's new socket, and
/// the returned [`JoinedSession`] carries the token that socket was advertised
/// under. For a session on another machine the whole dial runs again — no
/// endpoint file for that session exists on this machine — through
/// [`redial_restarted_remote_session_with`], until the serving machine reaches
/// the restarted session, the restarted session refuses this build's protocol
/// version, or [`RESTART_WINDOW_DURATION`] passes. Each dial waits
/// [`REMOTE_RESTART_POLL_INTERVAL_DURATION`] before it starts, the first dial
/// included.
///
/// `connection_token` is the token the connection that ended was opened
/// under; on this machine the wait watches the endpoint file for another one.
/// `remote_link_timeout_ms` goes to [`dial_remote`] for a session on a server.
/// The returned resume token is the secret the attach this client came back
/// on minted.
///
/// The Attach names `client_id` and presents `resume_token`, the secret the
/// attach of the connection that ended minted, as the proof of that claim. It
/// carries `placement_preview_target`, the placement target the client
/// previews. A session that still holds that record hands its view straight back. The
/// returned client is `client_id` when the session handed its record back,
/// and a fresh client the session minted when it cannot: a restart that could
/// not bring the carried layout back holds no record, a record whose tab
/// closed is not handed back, and a claim with no token or another token is
/// not proven. Example: a record whose tab closed is taken over by its token,
/// and the fresh client comes back with that record's view on the first tab.
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
#[allow(clippy::too_many_arguments)]
fn reconnect_after_restart(
    home: &Home,
    session_id: SessionId,
    client_id: ClientId,
    connection_token: &ConnectionToken,
    resume_token: Option<&ConnectionToken>,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
    remote_link_timeout_ms: u32,
) -> Result<JoinedSession, AttachmentEnding> {
    let restart_deadline = Instant::now() + RESTART_WINDOW_DURATION;
    match home {
        Home::Local {
            runtime_directory, ..
        } => {
            let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
            let router_connection_token_before_restart =
                EndpointFile::load_from_path(&router_endpoint_path)
                    .ok()
                    .map(|router_endpoint| router_endpoint.connection_token);
            let rejoin_result = rejoin_session(
                runtime_directory,
                session_id,
                client_id,
                connection_token,
                resume_token,
                restart_deadline,
                attach_settings,
                cell_size,
                placement_preview_target,
            );
            if rejoin_result.as_ref().is_err_and(|rejoin_ending| {
                *rejoin_ending == AttachmentEnding::RestartedIntoIncompatibleBuild
            }) {
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
            }
            rejoin_result
        }
        Home::Remote { server } => redial_restarted_remote_session_with(
            || {
                dial_remote(
                    server,
                    &SessionSelector::SessionId(session_id),
                    Some(client_id),
                    resume_token,
                    attach_settings,
                    cell_size,
                    placement_preview_target,
                    remote_link_timeout_ms,
                )
            },
            restart_deadline,
        ),
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
            Ok(joined_session) => return Ok(joined_session),
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

/// Whether a pause of `pause_duration`, begun `elapsed_duration` after the
/// link dropped, ends inside [`REDIAL_WINDOW_DURATION`].
///
/// `elapsed_duration` 111 seconds with an 8-second `pause_duration` ends at
/// 119 and fits; 112 seconds with the same `pause_duration` ends at 120 and
/// does not.
fn can_redial_pause_fit(elapsed_duration: Duration, pause_duration: Duration) -> bool {
    elapsed_duration + pause_duration < REDIAL_WINDOW_DURATION
}

/// The wait one failed redial hands the next: `current_wait_duration`
/// doubled, held at [`MAX_REDIAL_WAIT_DURATION`].
///
/// From [`FIRST_REDIAL_WAIT_DURATION`] that walks 1 second → 2 → 4 → 8 → 8,
/// and stays at 8 seconds however many dials follow.
fn compute_next_redial_wait(current_wait_duration: Duration) -> Duration {
    (current_wait_duration * 2).min(MAX_REDIAL_WAIT_DURATION)
}

/// When a remote viewer whose link dropped dials its server again, and what its
/// tag reads at each moment.
///
/// The first dial goes out [`FIRST_REDIAL_WAIT_DURATION`] after the link
/// dropped, and each failed dial widens the pause before the next as
/// [`compute_next_redial_wait`] says: 1 second, 2, 4, 8, then 8. A pause that
/// would end on or past [`REDIAL_WINDOW_DURATION`] from the drop is not taken.
/// The countdown is read from the next dial's deadline at each moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RedialSchedule {
    /// When the link dropped. The redial window counts from here.
    started_at: Instant,
    /// The dial that runs or comes next, counting from 1.
    attempt: u32,
    /// The pause taken before the dial `attempt` names.
    retry_wait: Duration,
    /// When the dial `attempt` names goes out, or `None` while it runs.
    next_dial_at: Option<Instant>,
}

impl RedialSchedule {
    /// The schedule of a link that dropped at `link_dropped_at`: attempt 1,
    /// going out [`FIRST_REDIAL_WAIT_DURATION`] after the drop.
    fn from_link_drop_at(link_dropped_at: Instant) -> Self {
        RedialSchedule {
            started_at: link_dropped_at,
            attempt: 1,
            retry_wait: FIRST_REDIAL_WAIT_DURATION,
            next_dial_at: Some(link_dropped_at + FIRST_REDIAL_WAIT_DURATION),
        }
    }

    /// The tag state at `current_time`: the attempt, and the whole seconds left
    /// before it goes out, rounded up, or `None` while it runs. 2.3 seconds
    /// left reads `retry in 3s`, and 2 seconds left reads `retry in 2s`.
    fn build_reconnecting(&self, current_time: Instant) -> Reconnecting {
        Reconnecting {
            attempt: self.attempt,
            retry_in_seconds: self.next_dial_at.map(|next_dial_at| {
                let remaining_duration = next_dial_at.saturating_duration_since(current_time);
                let whole_second_count =
                    remaining_duration.as_secs() + u64::from(remaining_duration.subsec_nanos() > 0);
                u32::try_from(whole_second_count).unwrap_or(u32::MAX)
            }),
        }
    }

    /// How long after `current_time` the tag reads another number of seconds
    /// or the next dial is due, whichever comes first. `None` while a dial
    /// runs. 2.3 seconds left wakes after 0.3 seconds, and 2 seconds left
    /// wakes after 1 second.
    fn compute_next_wakeup(&self, current_time: Instant) -> Option<Duration> {
        let remaining_duration = self.next_dial_at?.saturating_duration_since(current_time);
        let fractional_second = Duration::from_nanos(u64::from(remaining_duration.subsec_nanos()));
        if fractional_second.is_zero() {
            Some(remaining_duration.min(Duration::from_secs(1)))
        } else {
            Some(fractional_second)
        }
    }

    /// Mark the dial due at `current_time` as running and return `true`.
    /// Returns `false`, changing nothing, while a dial runs or before the next
    /// one is due.
    fn start_due_dial(&mut self, current_time: Instant) -> bool {
        let is_dial_due = self
            .next_dial_at
            .is_some_and(|next_dial_at| next_dial_at <= current_time);
        if is_dial_due {
            self.next_dial_at = None;
        }
        is_dial_due
    }

    /// Schedule the next dial after the running one failed at
    /// `dial_failed_at`: the pause widens as [`compute_next_redial_wait`] says,
    /// and the attempt counts up. Returns `false`, changing nothing, when that
    /// pause does not end inside [`REDIAL_WINDOW_DURATION`] from `started_at`,
    /// as [`can_redial_pause_fit`] says.
    fn schedule_next_dial(&mut self, dial_failed_at: Instant) -> bool {
        let retry_wait = compute_next_redial_wait(self.retry_wait);
        if !can_redial_pause_fit(
            dial_failed_at.saturating_duration_since(self.started_at),
            retry_wait,
        ) {
            return false;
        }
        self.retry_wait = retry_wait;
        self.attempt += 1;
        self.next_dial_at = Some(dial_failed_at + retry_wait);
        true
    }
}

/// How the loop gets a link back after the one it held ended.
#[derive(Debug)]
enum LinkRecovery {
    /// A remote viewer dials its server again on this schedule. Input typed
    /// meanwhile is dropped.
    Redial(RedialSchedule),
    /// The viewer joins its restarted session again. Input typed meanwhile is
    /// held in `input_hold` and sent once the client joins. `None` once the
    /// hold overflowed: everything typed during this rejoin is dropped.
    RestartRejoin {
        /// The held input, or `None` after an overflow.
        input_hold: Option<InputHold>,
    },
}

impl LinkRecovery {
    /// Hold `held_input` for a restarted session, oldest first. A redial drops
    /// it, and so does a rejoin while dock/undock mode, pane placement mode,
    /// the move chooser, or floating pane move mode is on for `client`. An
    /// input past [`MAX_HELD_INPUT_COUNT`] keys, fired bindings and pastes, or
    /// past [`MAX_FRAME_BYTE_COUNT`] bytes of paste text, drops everything
    /// held, and everything typed after it during this rejoin.
    ///
    /// A held [`HeldInput::LockCommand`] also switches `client` to its lock
    /// mode, as [`Client::apply_lock_command`] says. A lock command that is
    /// dropped switches nothing, and a drop of everything held puts back the
    /// lock mode and the pending lock command that `client` had before the
    /// first lock command this hold took.
    fn hold_input(&mut self, client: &mut Client, held_input: HeldInput) {
        let LinkRecovery::RestartRejoin { input_hold } = self else {
            return;
        };
        if client.find_viewer_local_input_mode().is_some() {
            return;
        }
        let Some(holding_input_hold) = input_hold else {
            return;
        };
        let held_lock_command = match &held_input {
            HeldInput::LockCommand {
                lock_command_id,
                lock_mode,
            } => Some((*lock_command_id, *lock_mode)),
            HeldInput::Keyboard(_) | HeldInput::DispatchPlan(_) | HeldInput::Paste(_) => None,
        };
        if !holding_input_hold.hold_input(held_input) {
            tracing::warn!(
                "more input arrived while the session restarted than this client holds; \
                 everything typed during the restart is dropped"
            );
            if let Some((lock_mode, pending_lock_command_id)) =
                holding_input_hold.lock_state_before_first_lock_command
            {
                client.set_lock_mode(lock_mode);
                client.set_pending_lock_command_id(pending_lock_command_id);
            }
            *input_hold = None;
            return;
        }
        let Some((lock_command_id, lock_mode)) = held_lock_command else {
            return;
        };
        holding_input_hold
            .lock_state_before_first_lock_command
            .get_or_insert((client.get_lock_mode(), client.get_pending_lock_command_id()));
        client.apply_lock_command(lock_command_id, lock_mode);
    }
}

/// The keys, fired bindings and pastes a restart rejoin holds, in the order
/// they were typed.
#[derive(Debug, Default)]
struct InputHold {
    /// The held input, oldest first.
    held_inputs: Vec<HeldInput>,
    /// The keys, fired bindings and pastes in `held_inputs`: a
    /// [`HeldInput::Keyboard`] counts each of its keys. At most
    /// [`MAX_HELD_INPUT_COUNT`].
    held_input_count: usize,
    /// The bytes of paste text in `held_inputs`. At most
    /// [`MAX_FRAME_BYTE_COUNT`].
    held_paste_byte_count: usize,
    /// The viewer's lock mode and pending lock command id from before the
    /// first [`HeldInput::LockCommand`] this hold took, or `None` while it
    /// took none.
    lock_state_before_first_lock_command: Option<(LockMode, Option<CommandId>)>,
}

impl InputHold {
    /// Add `held_input` after the inputs already held and return `true`.
    /// Returns `false`, holding nothing more, when the hold would then carry
    /// more than [`MAX_HELD_INPUT_COUNT`] keys, fired bindings and pastes, or
    /// more than [`MAX_FRAME_BYTE_COUNT`] bytes of paste text. A
    /// [`HeldInput::Keyboard`] of 2 keys added to 1023 held inputs is refused.
    fn hold_input(&mut self, held_input: HeldInput) -> bool {
        let (added_input_count, paste_byte_count) = match &held_input {
            HeldInput::Keyboard(key_inputs) => (key_inputs.len(), 0),
            HeldInput::Paste(pasted_text) => (1, pasted_text.len()),
            HeldInput::DispatchPlan(_) | HeldInput::LockCommand { .. } => (1, 0),
        };
        let held_input_count = self.held_input_count.saturating_add(added_input_count);
        let held_paste_byte_count = self.held_paste_byte_count.saturating_add(paste_byte_count);
        if held_input_count > MAX_HELD_INPUT_COUNT
            || held_paste_byte_count > MAX_FRAME_BYTE_COUNT as usize
        {
            return false;
        }
        self.held_input_count = held_input_count;
        self.held_paste_byte_count = held_paste_byte_count;
        self.held_inputs.push(held_input);
        true
    }
}

/// One input a restart rejoin holds, in the form it goes up once the client
/// joins.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HeldInput {
    /// The keys of one read the base keymap passed through, and the key
    /// releases, oldest first. Sent as one [`IpcRequestKind::Keyboard`].
    Keyboard(Vec<KeyInput>),
    /// The plan of a binding the base keymap fired, other than the quit
    /// binding and a lock binding. Run as [`Uplink::run_dispatch_plan`] runs
    /// it.
    DispatchPlan(DispatchPlan),
    /// A lock binding the base keymap fired. Sent as the `SetLockMode` command
    /// for `lock_mode` under `lock_command_id`.
    LockCommand {
        /// The command id the viewer waits for, minted when the binding fired.
        lock_command_id: CommandId,
        /// The lock mode the binding set on the viewer when it fired.
        lock_mode: LockMode,
    },
    /// Pasted text. Sent as [`IpcRequestKind::Paste`].
    Paste(String),
}

/// Run the stretch with no link: read this terminal, keep the tag on the
/// screen, run the attempts to get the link back, and hand back the session
/// one of them joined together with the input held for it.
///
/// `leftover_events` are read first, in order, then the loop's channel. Each
/// attempt runs `attempt_rejoin` on its own thread through
/// [`spawn_rejoin_attempt`], and its answer arrives on the channel. A restart
/// rejoin starts one attempt at once. A redial starts one each time its
/// [`RedialSchedule`] says a dial is due, and draws
/// `RECONNECTING (attempt 2, retry in 2s)`, then
/// `RECONNECTING (attempt 2, dialing)` while the dial runs. Each read waits
/// until the earliest of: the next tag change or due dial, the open key
/// sequence's ambiguity deadline, and the screen's image or animation wakeup.
///
/// Keys resolve against the base keymap, never the pane placement keymap. The
/// quit binding, including one an ambiguity deadline fires, returns
/// [`AttachmentEnding::LeftWhileReconnecting`] at once. A join that is still on
/// the channel then sends `Leaving { is_returning: true }` when the channel is
/// dropped, as [`JoinedSessionGuard`] states. Every other key and paste is held
/// through [`LinkRecovery::hold_input`] for a restart rejoin, and dropped for a
/// redial, as [`hold_fired_binding`] says for a binding. The keys of one read
/// that go to the pane are held as one [`HeldInput::Keyboard`], which ends at
/// any event or key outcome that is not such a key; a key that opens a
/// sequence keeps its breadcrumb. A held lock binding switches the lock mode
/// the next key resolves in: `<C-l>` then `<C-l>` holds `SetLockMode` for
/// `Locked`, then for `Normal`. Each input event is judged at the instant the
/// input thread read it: an open sequence whose ambiguity deadline passed
/// before then fires before the event is handled, and no deadline is judged at
/// the clock while an event the input thread read is still on its way. Example:
/// `<C-y>` binds `core:new-tab` and opens `<C-y> a`, and a paste is read 600 ms
/// after `<C-y>` with a 500 ms chord timeout: the held `NewTab` comes before
/// the held paste. A frame from the connection that ended is dropped. A resize
/// and a mouse event are dropped and not recorded. A cell-size reply is taken
/// by `cell_size_query` and not sent. Focus lost ends what
/// [`Client::handle_outer_terminal_focus_lost`] names, the same as while the
/// link is up.
/// `client`'s link status follows the stretch and is [`LinkStatus::Linked`]
/// again when this returns; a join draws the screen once more without the
/// tag.
///
/// # Errors
/// [`AttachmentEnding::TerminalGone`] when this terminal hangs up.
/// [`AttachmentEnding::LeftWhileReconnecting`] for the quit binding. The ending
/// an attempt answered with [`RejoinAnswer::Ended`].
/// [`AttachmentEnding::LinkLost`] carrying the last unreachable cause once a
/// redial's next pause does not fit the window, and
/// [`AttachmentEnding::Restarting`] for an unreachable answer to a restart
/// rejoin.
#[allow(clippy::too_many_arguments)]
fn run_without_link<B: Backend>(
    mut link_recovery: LinkRecovery,
    leftover_events: Vec<Incoming>,
    attempt_rejoin: &Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
    client: &mut Client,
    screen: &mut Screen<B>,
    active_tab_id: Option<TabId>,
    uplink: &Uplink,
    cell_size_query: &mut terminal::CellSizeQuery,
    incoming_sender: &mpsc::SyncSender<Incoming>,
    incoming_receiver: &mpsc::Receiver<Incoming>,
) -> Result<(JoinedSession, Vec<HeldInput>), AttachmentEnding> {
    if let LinkRecovery::RestartRejoin { .. } = link_recovery {
        spawn_rejoin_attempt(Arc::clone(attempt_rejoin), incoming_sender.clone());
    }
    let mut leftover_events = leftover_events.into_iter();
    let rejoin_result = 'stretch: loop {
        let current_time = Instant::now();
        let link_status = match &mut link_recovery {
            LinkRecovery::Redial(redial_schedule) => {
                if redial_schedule.start_due_dial(current_time) {
                    spawn_rejoin_attempt(Arc::clone(attempt_rejoin), incoming_sender.clone());
                }
                LinkStatus::Redialing(redial_schedule.build_reconnecting(current_time))
            }
            LinkRecovery::RestartRejoin { .. } => LinkStatus::AwaitingRestartedSession,
        };
        client.set_link_status(link_status);
        let input_judge_time = client.get_input_read_clock().find_input_judge_time();
        if let Some(bound_action) = input_judge_time
            .and_then(|input_judge_time| client.expire_key_sequence(input_judge_time))
        {
            if hold_fired_binding(&mut link_recovery, client, uplink, bound_action).is_break() {
                break Err(AttachmentEnding::LeftWhileReconnecting);
            }
        }
        screen.refresh(client, active_tab_id);
        let incoming_event = match leftover_events.next() {
            Some(leftover_event) => leftover_event,
            None => {
                let wakeup_computed_at = Instant::now();
                let tag_wakeup_duration = match &link_recovery {
                    LinkRecovery::Redial(redial_schedule) => {
                        redial_schedule.compute_next_wakeup(wakeup_computed_at)
                    }
                    LinkRecovery::RestartRejoin { .. } => None,
                };
                let next_wakeup_duration = select_earliest_duration(
                    select_earliest_duration(
                        tag_wakeup_duration,
                        client.compute_next_key_wakeup(wakeup_computed_at),
                    ),
                    select_earliest_duration(
                        screen.compute_next_image_wakeup_at(wakeup_computed_at),
                        screen.compute_next_placement_animation_wakeup_at(
                            wakeup_computed_at,
                            compute_frame_interval(
                                client.get_client_config().max_frames_per_second,
                            ),
                        ),
                    ),
                );
                let receive_result = match next_wakeup_duration {
                    Some(wakeup_duration) => incoming_receiver.recv_timeout(wakeup_duration),
                    None => incoming_receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match receive_result {
                    Ok(incoming_event) => incoming_event,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(AttachmentEnding::ConnectionDied)
                    }
                }
            }
        };
        match incoming_event {
            // A frame read from the connection that ended.
            Incoming::Frame { .. } | Incoming::Painted { .. } | Incoming::ImageChunk { .. } => {}
            // The next iteration starts with the refresh that paints the result.
            Incoming::ImageOutputReady => {}
            Incoming::RejoinAnswer(rejoin_answer) => match *rejoin_answer {
                RejoinAnswer::Joined(joined_session_guard) => {
                    break Ok(joined_session_guard.release_joined_session())
                }
                RejoinAnswer::Ended(attachment_ending) => break Err(attachment_ending),
                RejoinAnswer::Unreachable(dial_failure) => match &mut link_recovery {
                    LinkRecovery::Redial(redial_schedule) => {
                        if !redial_schedule.schedule_next_dial(Instant::now()) {
                            tracing::warn!(%dial_failure, "could not join the session again");
                            break Err(AttachmentEnding::LinkLost(Box::new(dial_failure)));
                        }
                    }
                    LinkRecovery::RestartRejoin { .. } => {
                        tracing::warn!(%dial_failure, "could not reach the restarted session");
                        break Err(AttachmentEnding::Restarting);
                    }
                },
            },
            Incoming::Input(terminal_input) => {
                let TerminalInput {
                    runtime_events,
                    read_at,
                } = *terminal_input;
                if matches!(runtime_events.as_slice(), [RuntimeEvent::Quit]) {
                    break Err(AttachmentEnding::TerminalGone);
                }
                client.get_input_read_clock().note_input_taken(read_at);
                // A sequence whose deadline passed before this read fires
                // first, and the read's events are handled after it.
                if let Some(expired_bound_action) = client.expire_key_sequence(read_at) {
                    if hold_fired_binding(&mut link_recovery, client, uplink, expired_bound_action)
                        .is_break()
                    {
                        break Err(AttachmentEnding::LeftWhileReconnecting);
                    }
                }
                // The keys of this read bound for the pane, held as one input
                // when an event that is not such a key comes, and at the end
                // of the read.
                let mut pane_key_inputs = Vec::new();
                for runtime_event in runtime_events {
                    if !matches!(runtime_event, RuntimeEvent::KeyInput { .. }) {
                        hold_pane_key_inputs(&mut link_recovery, client, &mut pane_key_inputs);
                    }
                    match runtime_event {
                        RuntimeEvent::KeyInput { key_input, .. } => {
                            let Some(chord) = key_input.to_binding_chord() else {
                                pane_key_inputs.push(key_input);
                                continue;
                            };
                            match client.resolve_key(chord, read_at) {
                                KeyOutcome::Fire(bound_action) => {
                                    hold_pane_key_inputs(
                                        &mut link_recovery,
                                        client,
                                        &mut pane_key_inputs,
                                    );
                                    if hold_fired_binding(
                                        &mut link_recovery,
                                        client,
                                        uplink,
                                        bound_action,
                                    )
                                    .is_break()
                                    {
                                        break 'stretch Err(
                                            AttachmentEnding::LeftWhileReconnecting,
                                        );
                                    }
                                }
                                KeyOutcome::PassThrough(_) => pane_key_inputs.push(key_input),
                                KeyOutcome::Pending | KeyOutcome::Discard => {
                                    hold_pane_key_inputs(
                                        &mut link_recovery,
                                        client,
                                        &mut pane_key_inputs,
                                    );
                                }
                            }
                        }
                        RuntimeEvent::HostPaste { pasted_text, .. } => {
                            link_recovery.hold_input(client, HeldInput::Paste(pasted_text));
                        }
                        RuntimeEvent::CellSize { cell_size, .. } => {
                            let _ = cell_size_query.accept_cell_size_reply(cell_size);
                        }
                        RuntimeEvent::OuterTerminalFocusLost { .. } => {
                            client.handle_outer_terminal_focus_lost();
                        }
                        // A resize and a mouse event are dropped and not
                        // recorded; the new link reads the terminal's size
                        // itself.
                        _ => {}
                    }
                }
                hold_pane_key_inputs(&mut link_recovery, client, &mut pane_key_inputs);
            }
        }
    };
    client.set_link_status(LinkStatus::Linked);
    let joined_session = rejoin_result?;
    screen.refresh(client, active_tab_id);
    let held_inputs = match link_recovery {
        LinkRecovery::RestartRejoin {
            input_hold: Some(input_hold),
        } => input_hold.held_inputs,
        LinkRecovery::RestartRejoin { input_hold: None } | LinkRecovery::Redial(_) => Vec::new(),
    };
    Ok((joined_session, held_inputs))
}

/// Run `attempt_rejoin` on its own `koshi-attach-rejoin` thread, and put its
/// answer on the loop's channel through `incoming_sender` as
/// [`Incoming::RejoinAnswer`].
///
/// A panic in `attempt_rejoin` answers [`RejoinAnswer::Ended`] carrying
/// [`AttachmentEnding::LinkLost`]. An answer that the loop no longer reads is
/// dropped: a connection it joined sends `Leaving { is_returning: true }`,
/// then closes, as [`JoinedSessionGuard`] states.
fn spawn_rejoin_attempt(
    attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
    incoming_sender: mpsc::SyncSender<Incoming>,
) {
    let _ = thread::Builder::new()
        .name("koshi-attach-rejoin".to_string())
        .spawn(move || {
            let rejoin_answer =
                catch_unwind(AssertUnwindSafe(|| attempt_rejoin())).unwrap_or_else(|_| {
                    RejoinAnswer::Ended(AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
                        detail: String::from(
                            "the attempt to reach the session again stopped on an internal error",
                        ),
                    })))
                });
            let _ = incoming_sender.send(Incoming::RejoinAnswer(Box::new(rejoin_answer)));
        })
        .expect("spawn attach rejoin thread");
}

/// The answer one redial gives the loop: [`RejoinAnswer::Joined`] for a dial
/// that joined; [`RejoinAnswer::Ended`] carrying [`AttachmentEnding::LinkLost`]
/// with the refusal for a [`DialError::Refused`], which every identical dial
/// gets again; and [`RejoinAnswer::Unreachable`] for a
/// [`DialError::Unreachable`] or a [`DialError::Restarting`].
fn build_redial_answer(dial_result: Result<JoinedSession, DialError>) -> RejoinAnswer {
    match dial_result {
        Ok(joined_session) => {
            RejoinAnswer::Joined(JoinedSessionGuard::from_joined_session(joined_session))
        }
        Err(DialError::Refused(dial_refusal)) => {
            tracing::warn!(%dial_refusal, "the server refused this client's dial");
            RejoinAnswer::Ended(AttachmentEnding::LinkLost(Box::new(dial_refusal)))
        }
        Err(DialError::Unreachable(dial_failure) | DialError::Restarting(dial_failure)) => {
            RejoinAnswer::Unreachable(dial_failure)
        }
    }
}

/// Hold `pane_key_inputs`, the keys of one read that go to the pane, as one
/// [`HeldInput::Keyboard`] through [`LinkRecovery::hold_input`], and leave it
/// empty. Holds nothing when it has no key.
fn hold_pane_key_inputs(
    link_recovery: &mut LinkRecovery,
    client: &mut Client,
    pane_key_inputs: &mut Vec<KeyInput>,
) {
    if pane_key_inputs.is_empty() {
        return;
    }
    link_recovery.hold_input(client, HeldInput::Keyboard(take(pane_key_inputs)));
}

/// Answer one binding that fired while this viewer has no link, against the
/// action table. Returns [`ControlFlow::Break`] for the quit binding, holding
/// nothing, and [`ControlFlow::Continue`] for every other binding.
///
/// A lock binding is held as [`HeldInput::LockCommand`] for the lock mode it
/// sets from `client`'s lock mode, under a new command id. Every other binding
/// is held as its [`HeldInput::DispatchPlan`]. Both go through
/// [`LinkRecovery::hold_input`]. An action the table refuses is held as
/// nothing.
fn hold_fired_binding(
    link_recovery: &mut LinkRecovery,
    client: &mut Client,
    uplink: &Uplink,
    bound_action: BoundAction,
) -> ControlFlow<()> {
    let Some(dispatch_plan) = uplink.resolve_bound_action(client, &bound_action) else {
        return ControlFlow::Continue(());
    };
    if is_quit_dispatch_plan(&dispatch_plan) {
        return ControlFlow::Break(());
    }
    let held_input = match find_lock_mode_set_by(&dispatch_plan, client.get_lock_mode()) {
        Some(lock_mode) => HeldInput::LockCommand {
            lock_command_id: CommandId::new(),
            lock_mode,
        },
        None => HeldInput::DispatchPlan(dispatch_plan),
    };
    link_recovery.hold_input(client, held_input);
    ControlFlow::Continue(())
}

/// Send what a restart rejoin held up a new connection, oldest first: the keys
/// of one read as one [`IpcRequestKind::Keyboard`], a binding's plan through
/// [`Uplink::run_dispatch_plan`] with no painted frame, a lock command as `SetLockMode` under its
/// own command id, and a paste as [`IpcRequestKind::Paste`].
///
/// `client` stops waiting for a lock command sent on the connection that
/// ended, and waits for the last held lock command sent here, as
/// [`Client::set_pending_lock_command_id`] says. With no held lock command,
/// the new connection's first frame sets the lock mode. The lock mode each
/// held lock command set when it was held stays as it is.
fn send_held_inputs(client: &mut Client, uplink: &mut Uplink, held_inputs: Vec<HeldInput>) {
    client.set_pending_lock_command_id(None);
    for held_input in held_inputs {
        match held_input {
            HeldInput::Keyboard(key_inputs) => {
                uplink.send_request(IpcRequestKind::Keyboard { key_inputs });
            }
            HeldInput::DispatchPlan(dispatch_plan) => {
                uplink.run_dispatch_plan(client, dispatch_plan, None);
            }
            HeldInput::LockCommand {
                lock_command_id,
                lock_mode,
            } => {
                uplink.submit_key_binding_command(
                    client,
                    lock_command_id,
                    build_set_lock_mode_command(lock_mode),
                );
                client.set_pending_lock_command_id(Some(lock_command_id));
            }
            HeldInput::Paste(pasted_text) => {
                uplink.send_request(IpcRequestKind::Paste { pasted_text });
            }
        }
    }
}

/// Close `uplink`'s request queue behind the `core:quit` it holds, wait up to
/// [`MAX_QUIT_WRITE_WAIT_DURATION`] for its writer thread to hand every queued
/// request to the socket, and return [`AttachmentEnding::Detached`].
fn leave_after_quit(uplink: &mut Uplink) -> AttachmentEnding {
    if !uplink.drain_request_queue(Instant::now() + MAX_QUIT_WRITE_WAIT_DURATION) {
        tracing::warn!("the connection did not take the quit in time; leaving without it");
    }
    AttachmentEnding::Detached
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
    locally_measured_cell_size: Option<PixelCellSize>,
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

/// The session a listing settles on, picked from the sessions running for this
/// user and the rows in `remote`.
///
/// The local rows are the ones `koshi list-sessions` prints, from the same sweep
/// of the runtime directory, so nothing here probes anything that listing does
/// not. `remote` holds one row per session on a saved server that answered,
/// each carrying that server in its `server` field; a picker for a session
/// switch passes none. A single local row is the answer on its own.
/// Every other non-empty listing — several rows, or one row on a saved server —
/// is printed and the number typed on stdin selects the row. This runs before
/// the terminal enters raw mode, so the prompt is a plain stdin read.
///
/// When a session that is listening could not answer, a list of under two
/// local rows reports that session and settles on no row.
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
    let mut session_rows = discovery::build_session_rows(&discovered_sessions);
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
    let selected_session_row = &session_rows[selected_row_index];
    Ok(match selected_row_index.checked_sub(local_row_count) {
        Some(remote_row_index) => SessionSelection::Remote(remote_row_index),
        None => SessionSelection::Local(format_session_target(
            selected_session_row.session_id,
            selected_session_row.owner_user_id,
        )),
    })
}

/// Whether a listing of `session_row_count` rows, the first
/// `local_session_row_count` of them on this machine, settles on its only row
/// without asking: exactly one row, and that row is local. A single remote
/// row, and every longer listing, is asked.
fn is_single_local_session_selection(
    session_row_count: usize,
    local_session_row_count: usize,
) -> bool {
    session_row_count == 1 && local_session_row_count == 1
}

/// Which row a listing settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionSelection {
    /// A session on this machine, named by its id. A session that another
    /// user's folder of the shared directory advertises is named by
    /// `<user id>/<session id>`.
    Local(String),
    /// A session on a saved server: where in the listing's `remote` rows it sat.
    Remote(usize),
}

/// Where in `session_rows` a listing settles. A list of one row settles on it
/// without printing anything; a longer list is printed by
/// [`prompt_for_session_selection`] and the number typed on stdin names the row.
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

/// Print one numbered line per session — number, name, the text
/// [`format_session_target`] gives for its id and user id, and for a session
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
                format_session_target(session_row.session_id, session_row.owner_user_id)
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
/// Empty `session_rows` is [`CliError::NoSessions`]; a number outside
/// `1..=session_rows.len()`, and a line that is not a number, are
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
        RouterResult::Found(session_address) => Ok(session_address),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        unexpected_result => Err(CliError::IpcUnavailable {
            detail: format!(
                "the router answered an attach lookup with {}",
                unexpected_result.get_wire_name()
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

/// Join the session on an open connection, given as its two halves: write the
/// Hello and a `Prepare` carrying the Attach back to back on `writer`, then
/// read both replies in order on `reader`. Each half keeps the deadline the
/// caller set on it until then. Once the session answers `Prepared`, write the
/// `Commit`, numbered 3, and read the Attach's answer, both with
/// `commit_timeout` from the moment the `Commit` is written; `None` waits
/// however long it takes. Returns the client the server minted for this
/// terminal, the session it says that client joined, and the secret this
/// attach minted.
///
/// A join that ends before the `Commit` is written leaves no client behind in
/// the session.
///
/// The client names no identity of its own — the server mints the client id
/// and answers with it — so every value comes from the reply.
///
/// `resume_client_id` names the client record to come back as: a client returning after
/// the session replaced its own process image carries it, and a first join
/// leaves it `None`. `resume_token` is the secret that record's last attach
/// minted, presented as the proof of that claim, and a first join leaves it
/// `None`. The server hands that record back when it still holds it, the tab
/// that record was viewing still exists, no connection is streaming for it,
/// and the token is the one that record's last attach minted, and mints a
/// fresh client otherwise. Either way, the caller holds the returned id from
/// here. `placement_preview_target` is the placement target the client
/// previews, and a first join leaves it `None`.
///
/// # Errors
/// [`DialError::Unreachable`] when the Hello or the `Prepare` could not be
/// written, or a reply to them could not be read. [`DialError::Unreachable`]
/// carrying [`CliError::CommittedRequestUnanswered`] when the `Commit` could
/// not be written or its answer could not be read: the session may still
/// attach the client. [`DialError::Restarting`] carrying
/// [`CliError::SessionRestarting`] when the session refused the `Prepare` or
/// the `Commit` while it replaces its process image: the session holds no
/// client for this join. [`DialError::Refused`] for every other reply that
/// arrived and does not join: a refused Hello, a protocol version this build
/// does not accept, a refused `Prepare` or Attach, and an answer of another
/// kind, which carries [`CliError::CommittedRequestUnanswered`] after the
/// `Commit`.
#[allow(clippy::too_many_arguments)]
fn join_session(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    connection_token: &ConnectionToken,
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
    commit_timeout: Option<Duration>,
) -> Result<(ClientId, SessionId, Option<ConnectionToken>), DialError> {
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(connection_token.clone()),
    };
    writer.send(&hello).map_err(build_link_failure)?;
    writer
        .send(&build_attach_prepare_request(
            resume_client_id,
            resume_token,
            attach_settings,
            cell_size,
            None,
            placement_preview_target,
        ))
        .map_err(build_link_failure)?;

    talk::parse_session_hello_answer(reader.recv_answer().map_err(build_link_failure)?)
        .map_err(DialError::Refused)?;
    validate_attach_prepare_answer(reader.recv_answer().map_err(build_link_failure)?)?;
    let attach_result = ipc_client::commit_prepared_request(reader, writer, 3, commit_timeout)
        .map_err(DialError::Unreachable)?;
    parse_attached_session(attach_result).map_err(DialError::from_session_refusal)
}

/// What this terminal asks of the session at every attach, besides the cell
/// size each dial measures again: the image protocols its terminal proved it
/// can receive, and its `max-frames-per-second`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachSettings {
    /// The image protocols the terminal proved it can receive.
    graphics_support: terminal::GraphicsSupport,
    /// The most frames a second the session sends this terminal, from `30`
    /// to `120`.
    max_frames_per_second: u16,
}

/// The `Prepare` this client writes, numbered 2, carrying its Attach: the
/// request that follows the Hello on every connection into a session.
///
/// `resume_client_id` names the client record to come back as, and is `None` on a first
/// join. `resume_token` is the secret the last attach minted, presented to get
/// that attach's view back, and is `None` on a first join and whenever no
/// token was minted. `link_timeout_ms` is `remote-link-timeout-ms` on a link
/// to another machine, and the session writes this client a `Pong` whenever it
/// has written it nothing for a third of it. It is `None` on this machine,
/// where the session writes no `Pong`. `placement_preview_target` is the
/// placement target the client previews, and is `None` on a first join.
/// Reports the pane area left by the built-in two-row UI.
fn build_attach_prepare_request(
    resume_client_id: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    link_timeout_ms: Option<u32>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
) -> IpcRequest {
    let viewport_size = get_terminal_viewport_size();
    IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Prepare(Box::new(IpcRequestKind::Attach {
            viewport_size,
            resume_client_id,
            resume_token: resume_token.cloned(),
            pane_area: Some(compute_core_pane_area(viewport_size)),
            graphics_capabilities: attach_settings
                .graphics_support
                .build_graphics_capabilities(),
            cell_size,
            link_timeout_ms,
            max_frames_per_second: attach_settings.max_frames_per_second,
            placement_preview_target,
        })),
    }
}

/// The client the server minted for this terminal, the session it says that
/// client joined, and the secret this attach minted, out of `attach_result`,
/// the answer to the `Commit` that ran this client's Attach.
///
/// An answer that echoes no pane area is taken as it stands, and logs one
/// debug line.
///
/// # Errors
/// What [`talk::build_peer_refusal_error`] gives for a refusal, and
/// [`CliError::CommittedRequestUnanswered`] naming the reply for an answer of
/// another kind: the session may hold a client for this attach.
fn parse_attached_session(
    attach_result: IpcResult,
) -> Result<(ClientId, SessionId, Option<ConnectionToken>), CliError> {
    match attach_result {
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
        unexpected_result => Err(CliError::CommittedRequestUnanswered {
            detail: talk::SESSION_PEER_WORDS
                .format_unexpected_reply_detail(unexpected_result.get_wire_name()),
        }),
    }
}

/// Come back to a session that is replacing its own process image: wait for its
/// new socket, connect to it, and join again as `client_id`, presenting
/// `resume_token`, all before `restart_deadline`. Returns the connection
/// joined, carrying the client the session answered with, the token of the
/// new socket, and the resume token the join minted. The connection carries no
/// deadline once joined.
///
/// The client is `client_id` when the session handed that record back, and a
/// fresh client the session minted otherwise.
///
/// The Attach carries `placement_preview_target`, the placement target the
/// client previews.
///
/// `connection_token` is the token this client attached under; the wait watches it for a
/// change. A new socket whose session refuses the join while it replaces its
/// process image once more is left the same way: the wait watches that
/// socket's token for a change. The connect, the writes of the Hello and of
/// the `Prepare` carrying the Attach, and the reads of both replies each end
/// at `restart_deadline`. The `Commit` that runs the Attach and the read of
/// its answer end [`RESTART_WINDOW_DURATION`] after the `Commit` starts to be
/// written.
///
/// `Err` carries the ending the attachment reports.
/// [`AttachmentEnding::RestartedIntoIncompatibleBuild`] when the new socket
/// refuses this build's protocol version.
/// [`AttachmentEnding::RefusedAfterRestart`] carrying the refusal when the new
/// socket answers the join with any other refusal.
/// [`AttachmentEnding::Restarting`] for every other way the client cannot come
/// back: another local user's session, which advertises no endpoint file this
/// user can read; a session that has not come back, or not answered the join,
/// by `restart_deadline`; and a new socket that refuses the connection, or that
/// breaks before it answers the join.
#[allow(clippy::too_many_arguments)]
fn rejoin_session(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    connection_token: &ConnectionToken,
    resume_token: Option<&ConnectionToken>,
    restart_deadline: Instant,
    attach_settings: AttachSettings,
    cell_size: Option<PixelCellSize>,
    placement_preview_target: Option<PanePlacementPreviewTarget>,
) -> Result<JoinedSession, AttachmentEnding> {
    if connection_token.expose_secret().is_empty() {
        tracing::warn!(
            %session_id,
            "another local user's session is restarting, and this user cannot read its endpoint file"
        );
        return Err(AttachmentEnding::Restarting);
    }
    let mut replaced_connection_token = connection_token.clone();
    loop {
        let Some(endpoint) = ipc_client::wait_for_new_session_endpoint(
            runtime_directory,
            session_id,
            Some(&replaced_connection_token),
            restart_deadline,
        ) else {
            tracing::warn!(%session_id, "the session advertised no new socket after its restart");
            return Err(AttachmentEnding::Restarting);
        };
        let (mut reader, mut writer) =
            ipc_client::connect_to_session(&endpoint, session_id, Some(restart_deadline))
                .map_err(|connection_error| {
                    tracing::warn!(%connection_error, "could not reach the restarted session");
                    AttachmentEnding::Restarting
                })?
                .split();
        reader.set_deadline(Some(restart_deadline));
        writer.set_deadline(Some(restart_deadline));
        let join_result = join_session(
            &mut reader,
            &mut writer,
            &endpoint.connection_token,
            Some(client_id),
            resume_token,
            attach_settings,
            cell_size,
            placement_preview_target,
            Some(RESTART_WINDOW_DURATION),
        );
        let (rejoined_client_id, rejoined_session_id, minted_resume_token) = match join_result {
            Ok(joined_client) => joined_client,
            Err(DialError::Restarting(join_refusal)) => {
                tracing::info!(%join_refusal, "the restarted session is replacing its image again");
                replaced_connection_token = endpoint.connection_token;
                continue;
            }
            Err(DialError::Refused(join_refusal @ CliError::ProtocolVersionRefused { .. })) => {
                tracing::warn!(%join_refusal, "the restarted session refused this client");
                return Err(AttachmentEnding::RestartedIntoIncompatibleBuild);
            }
            Err(DialError::Refused(join_refusal)) => {
                tracing::warn!(%join_refusal, "the restarted session refused this client");
                return Err(AttachmentEnding::RefusedAfterRestart(Box::new(
                    join_refusal,
                )));
            }
            Err(DialError::Unreachable(join_failure)) => {
                tracing::warn!(%join_failure, "could not reach the restarted session");
                return Err(AttachmentEnding::Restarting);
            }
        };
        reader.set_deadline(None);
        writer.set_deadline(None);
        return Ok(JoinedSession {
            reader,
            writer,
            client_id: rejoined_client_id,
            session_id: rejoined_session_id,
            connection_token: endpoint.connection_token,
            resume_token: minted_resume_token,
        });
    }
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
/// The thread owns the connection's [`PaintedUpdateReceiver`]. Each painted
/// update is applied here, and its last part puts one [`Incoming::Painted`]
/// on the channel. A painted update that does not decode or apply is a
/// failed read. Each image chunk frame goes on the channel as one
/// [`Incoming::ImageChunk`] that holds the frame as read.
///
/// A frame this build has no variant for, JSON or binary, is dropped here and
/// never reaches the loop. It comes from a newer session server, and the
/// frames around it still draw.
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
        .spawn(move || {
            let mut painted_update_receiver = PaintedUpdateReceiver::new();
            loop {
                let session_frame_result = frame_reader
                    .recv_message_or_binary::<IncomingEvent>()
                    .and_then(|received_frame| {
                        painted_update_receiver.accept_session_frame(received_frame)
                    });
                let incoming_frame = match session_frame_result {
                    Ok(None) => continue,
                    Ok(Some(SessionFrame::Event(MaybeKnown::Known(session_event)))) => {
                        Incoming::Frame {
                            connection_index,
                            session_event_result: Ok(session_event),
                        }
                    }
                    Ok(Some(SessionFrame::Event(MaybeKnown::Unknown { variant_name }))) => {
                        tracing::debug!(%variant_name, "session frame this build does not have");
                        continue;
                    }
                    Ok(Some(SessionFrame::Painted(received_painted_frame))) => Incoming::Painted {
                        connection_index,
                        received_painted_frame,
                    },
                    Ok(Some(SessionFrame::ImageChunk(image_chunk_frame))) => Incoming::ImageChunk {
                        connection_index,
                        image_chunk_frame,
                    },
                    Err(read_error) => Incoming::Frame {
                        connection_index,
                        session_event_result: Err(read_error),
                    },
                };
                let is_connection_broken = matches!(
                    incoming_frame,
                    Incoming::Frame {
                        session_event_result: Err(_),
                        ..
                    }
                );
                if incoming_sender.send(incoming_frame).is_err() || is_connection_broken {
                    break;
                }
            }
        })
        .expect("spawn attach reader thread");
}

/// Write the loop's requests on their own thread and give back the queue they
/// are handed to, together with the signal of the thread's end.
///
/// A write on this thread blocks until the session has taken the bytes. The
/// loop only puts the request on the queue: a session that reads slowly backs
/// the queue up, and the loop goes on reading this terminal's input.
///
/// Requests leave in the order they were queued and nothing here is folded or
/// reordered: a [`WireMouseAction::Forward`] is one report the pane's program
/// must see, and every request carries the `request_id` the session answers
/// under. The pile the loop holds is where folding happens, in
/// [`coalesce_mouse_actions`].
///
/// A request over the frame cap — a paste of more text than one frame carries —
/// is refused with nothing written, and that request alone is dropped; the next
/// one goes out. Any other failed write ends the thread, a write past the idle
/// timeout of `writer` included.
fn spawn_uplink_writer(mut writer: FrameWriter) -> UplinkWriter {
    let (request_sender, request_receiver) = mpsc::channel::<IpcRequest>();
    let (writer_end_sender, writer_end_receiver) = mpsc::channel::<()>();
    let _ = thread::Builder::new()
        .name("koshi-attach-writer".to_string())
        .spawn(move || {
            // Dropped when the thread ends, which disconnects
            // `writer_end_receiver`.
            let _writer_end_sender = writer_end_sender;
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
    UplinkWriter {
        request_sender,
        writer_end_receiver,
    }
}

/// Move every event the terminal-input thread produces onto the loop's own
/// channel, so the session's frames and this terminal's input arrive on one
/// receiver.
fn spawn_input_relay(
    input_receiver: mpsc::Receiver<TerminalInput>,
    incoming_sender: mpsc::SyncSender<Incoming>,
) {
    let _ = thread::Builder::new()
        .name("koshi-attach-input".to_string())
        .spawn(move || {
            for terminal_input in input_receiver {
                if incoming_sender
                    .send(Incoming::Input(Box::new(terminal_input)))
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn attach input relay thread");
}

/// How long the console control handler holds the process open on Windows
/// while the attachment ends: 3 seconds. Windows ends the process when the
/// handler returns, and 5 seconds after a console close at the latest.
#[cfg(windows)]
const CONSOLE_CLOSE_WAIT_DURATION: Duration = Duration::from_secs(3);

/// The channel of the attachment that a console control event reaches on
/// Windows, or `None` while no [`TerminalCloseWatch`] runs.
#[cfg(windows)]
static CONSOLE_CLOSE_INCOMING_SENDER: std::sync::Mutex<Option<mpsc::SyncSender<Incoming>>> =
    std::sync::Mutex::new(None);

/// Whether a Ctrl+C or Ctrl+Break signal reached [`handle_console_control`]
/// while a [`TerminalCloseWatch`] ran, and [`end_process_after_termination_signal`]
/// has not ended the process for it yet.
#[cfg(windows)]
static HAS_RECEIVED_CONSOLE_BREAK: AtomicBool = AtomicBool::new(false);

/// The signals that end `koshi attach` on Linux and macOS.
#[cfg(unix)]
const TERMINATION_SIGNALS: [libc::c_int; 4] =
    [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];

/// The actions this process registers for the [`TERMINATION_SIGNALS`], once
/// per signal, on Linux and macOS.
#[cfg(unix)]
static TERMINATION_SIGNAL_ACTIONS: LazyLock<TerminationSignalActions> =
    LazyLock::new(|| TerminationSignalActions {
        should_run_default_action: Arc::new(AtomicBool::new(true)),
        received_termination_signal: Arc::new(AtomicI32::new(0)),
        registered_signals: Mutex::new(Vec::new()),
    });

/// The two actions of each termination signal on Linux and macOS: the first
/// records the signal, and the second runs its default action while
/// `should_run_default_action` is `true`.
#[cfg(unix)]
struct TerminationSignalActions {
    /// `false` while a [`TerminalCloseWatch`] runs and has taken no
    /// termination signal; `true` otherwise.
    should_run_default_action: Arc<AtomicBool>,
    /// The termination signal that arrived last since
    /// [`take_received_termination_signal`] last ran, or `0`.
    received_termination_signal: Arc<AtomicI32>,
    /// The termination signals whose two actions are registered.
    registered_signals: Mutex<Vec<libc::c_int>>,
}

#[cfg(unix)]
impl TerminationSignalActions {
    /// Register the two actions of `termination_signal`, and do nothing when
    /// they are already registered.
    ///
    /// # Errors
    /// The failure of `sigaction` to install signal-hook's handler.
    fn register_signal_actions(&self, termination_signal: libc::c_int) -> io::Result<()> {
        let mut registered_signals = self
            .registered_signals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if registered_signals.contains(&termination_signal) {
            return Ok(());
        }
        let received_termination_signal = Arc::clone(&self.received_termination_signal);
        // SAFETY: the action runs one store to an atomic integer, which is
        // async-signal-safe. It does not allocate, lock or panic.
        unsafe {
            signal_hook::low_level::register(termination_signal, move || {
                received_termination_signal.store(termination_signal, Ordering::SeqCst);
            })?;
        }
        signal_hook::flag::register_conditional_default(
            termination_signal,
            Arc::clone(&self.should_run_default_action),
        )?;
        registered_signals.push(termination_signal);
        Ok(())
    }
}

/// One attachment's watch for its terminal closing or its process being asked
/// to end: on Linux and macOS, every [`TERMINATION_SIGNALS`] entry this process
/// does not ignore, and on Windows, the console close, Ctrl+C and Ctrl+Break.
/// Each one puts [`RuntimeEvent::Quit`] on the loop's channel, and the loop
/// then ends as [`AttachmentEnding::TerminalGone`]. One watch runs at a time.
/// Dropping the watch stops it.
struct TerminalCloseWatch {
    /// Closes the thread that reads the termination signals.
    #[cfg(unix)]
    termination_signal_handle: signal_hook::iterator::Handle,
}

impl TerminalCloseWatch {
    /// Start the watch for the loop that reads `incoming_sender`'s channel.
    ///
    /// On Linux and macOS, each termination signal this process does not
    /// ignore gets its two [`TerminationSignalActions`], and the
    /// `koshi-attach-signal` thread reads it. The first signal the thread
    /// reads makes the next one run its default action at once. While no
    /// watch runs, a termination signal runs its default action. A signal
    /// this process ignores when the watch starts, as `nohup` makes it ignore
    /// `SIGHUP`, stays ignored, and a process that ignores all four gets no
    /// watch: the result is `None`.
    ///
    /// On Windows, `handle_console_control` takes the console close, Ctrl+C
    /// and Ctrl+Break.
    ///
    /// # Errors
    /// On Linux and macOS, the failure to read a signal's action, to register
    /// a signal, or to start the thread. On Windows, the failure of
    /// `SetConsoleCtrlHandler`.
    fn start_terminal_close_watch(
        incoming_sender: mpsc::SyncSender<Incoming>,
    ) -> io::Result<Option<TerminalCloseWatch>> {
        #[cfg(unix)]
        {
            let termination_signal_actions = &*TERMINATION_SIGNAL_ACTIONS;
            let mut watched_signals = Vec::new();
            for termination_signal in TERMINATION_SIGNALS {
                if is_signal_ignored(termination_signal)? {
                    continue;
                }
                termination_signal_actions.register_signal_actions(termination_signal)?;
                watched_signals.push(termination_signal);
            }
            if watched_signals.is_empty() {
                return Ok(None);
            }
            let mut termination_signals = signal_hook::iterator::Signals::new(&watched_signals)?;
            let termination_signal_handle = termination_signals.handle();
            let should_run_default_action =
                Arc::clone(&termination_signal_actions.should_run_default_action);
            termination_signal_actions
                .should_run_default_action
                .store(false, Ordering::SeqCst);
            let signal_thread = thread::Builder::new()
                .name("koshi-attach-signal".to_string())
                .spawn(move || {
                    for _ in termination_signals.forever() {
                        should_run_default_action.store(true, Ordering::SeqCst);
                        let _ = incoming_sender.send(Incoming::Input(Box::new(TerminalInput {
                            runtime_events: vec![RuntimeEvent::Quit],
                            read_at: Instant::now(),
                        })));
                    }
                });
            if let Err(thread_start_error) = signal_thread {
                termination_signal_actions
                    .should_run_default_action
                    .store(true, Ordering::SeqCst);
                return Err(thread_start_error);
            }
            Ok(Some(TerminalCloseWatch {
                termination_signal_handle,
            }))
        }
        #[cfg(windows)]
        {
            *CONSOLE_CLOSE_INCOMING_SENDER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(incoming_sender);
            // SAFETY: `handle_console_control` is a `HandlerRoutine` with the
            // signature `SetConsoleCtrlHandler` takes, and it lives for the
            // whole process.
            let is_handler_added = unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(handle_console_control),
                    1,
                )
            } != 0;
            if !is_handler_added {
                *CONSOLE_CLOSE_INCOMING_SENDER
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                return Err(io::Error::last_os_error());
            }
            Ok(Some(TerminalCloseWatch {}))
        }
    }
}

impl Drop for TerminalCloseWatch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            TERMINATION_SIGNAL_ACTIONS
                .should_run_default_action
                .store(true, Ordering::SeqCst);
            self.termination_signal_handle.close();
        }
        #[cfg(windows)]
        {
            // SAFETY: removes the handler `start_terminal_close_watch` added.
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(handle_console_control),
                    0,
                );
            }
            *CONSOLE_CLOSE_INCOMING_SENDER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }
}

/// Whether this process ignores `signal`: `true` when its action is `SIG_IGN`.
///
/// # Errors
/// The failure of `sigaction` to read the action.
#[cfg(unix)]
pub(crate) fn is_signal_ignored(signal: libc::c_int) -> io::Result<bool> {
    // SAFETY: an all-zero `sigaction` is valid.
    let mut signal_action: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a null new action makes `sigaction` only write the current
    // action of `signal` into `signal_action`.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut signal_action) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(signal_action.sa_sigaction == libc::SIG_IGN)
}

/// The termination signal that arrived since the last call, as
/// [`TerminationSignalActions`] recorded it, or `None`. The record is cleared.
#[cfg(unix)]
fn take_received_termination_signal() -> Option<libc::c_int> {
    let received_termination_signal = TERMINATION_SIGNAL_ACTIONS
        .received_termination_signal
        .swap(0, Ordering::SeqCst);
    (received_termination_signal != 0).then_some(received_termination_signal)
}

/// End this process for the termination signal an attachment took, and
/// return when it took none.
///
/// On Linux and macOS, the process ends by the default action of the signal
/// [`take_received_termination_signal`] gives, and exits with `128` plus the
/// signal number when that action does not end it. On Windows, after a Ctrl+C
/// or Ctrl+Break reached `handle_console_control`, the process exits with
/// `STATUS_CONTROL_C_EXIT` (`0xC000013A`).
fn end_process_after_termination_signal() {
    #[cfg(unix)]
    if let Some(received_termination_signal) = take_received_termination_signal() {
        let _ = signal_hook::low_level::emulate_default_handler(received_termination_signal);
        std::process::exit(128 + received_termination_signal);
    }
    #[cfg(windows)]
    if HAS_RECEIVED_CONSOLE_BREAK.swap(false, Ordering::SeqCst) {
        std::process::exit(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT);
    }
}

/// The console control handler on Windows.
///
/// A console close, Ctrl+C or Ctrl+Break puts [`RuntimeEvent::Quit`] on the
/// channel in [`CONSOLE_CLOSE_INCOMING_SENDER`] and waits
/// [`CONSOLE_CLOSE_WAIT_DURATION`]. A close then returns `1`, after which
/// Windows ends the process. A Ctrl+C or Ctrl+Break sets
/// [`HAS_RECEIVED_CONSOLE_BREAK`] first, and returns `0` after the wait, which
/// runs the default handler that ends the process.
///
/// `0` comes back at once, which passes the event to the next handler, for
/// every other control event, an event with no channel, a close whose loop
/// has ended, and a Ctrl+C or Ctrl+Break that arrives after one already set
/// [`HAS_RECEIVED_CONSOLE_BREAK`].
#[cfg(windows)]
unsafe extern "system" fn handle_console_control(control_type: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT};
    let is_console_break = control_type == CTRL_C_EVENT || control_type == CTRL_BREAK_EVENT;
    if !is_console_break && control_type != CTRL_CLOSE_EVENT {
        return 0;
    }
    let incoming_sender = CONSOLE_CLOSE_INCOMING_SENDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(incoming_sender) = incoming_sender else {
        return 0;
    };
    if is_console_break && HAS_RECEIVED_CONSOLE_BREAK.swap(true, Ordering::SeqCst) {
        return 0;
    }
    let is_quit_sent = incoming_sender
        .send(Incoming::Input(Box::new(TerminalInput {
            runtime_events: vec![RuntimeEvent::Quit],
            read_at: Instant::now(),
        })))
        .is_ok();
    if !is_quit_sent && !is_console_break {
        return 0;
    }
    thread::sleep(CONSOLE_CLOSE_WAIT_DURATION);
    if is_console_break {
        0
    } else {
        1
    }
}

/// Build the bounded queue shared by terminal input and the session reader.
fn build_incoming_channel() -> (mpsc::SyncSender<Incoming>, mpsc::Receiver<Incoming>) {
    mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)
}

/// Build the bounded queue between the terminal reader and the input relay.
fn build_input_channel() -> (
    mpsc::SyncSender<TerminalInput>,
    mpsc::Receiver<TerminalInput>,
) {
    mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)
}

/// Answer one event read from this terminal.
///
/// A key belongs to the viewer that received it: the keymap, the input mode and
/// any open sequence all live here. This decides what the press means, and
/// the session sees only the answer — the commands a binding runs, or the whole
/// key event to write. A release, and a key no keybinding can name, resolve
/// nothing and reach the session as they are. A key resolves at `read_at`, the
/// instant the input thread read it: a key that opens a sequence that is both a
/// binding and a longer one's prefix sets that sequence's ambiguity deadline
/// one chord timeout after `read_at`. A press that goes to the pane's program
/// also ends the selection gesture under way. A key that goes to the pane is
/// added to `pane_key_inputs`, and sends nothing. Every other key outcome
/// first sends `pane_key_inputs` as one [`IpcRequestKind::Keyboard`]
/// ([`send_pane_key_inputs`]): a fired binding, a key a sequence holds, and a
/// discarded key. The caller sends `pane_key_inputs` before any other event,
/// and at the end of the read. A resize records the viewport and
/// pane area and sends both to the session. Pasted text goes up whole and ends
/// the gesture too; the session writes it into the pane, bracketing it when
/// that pane asked for bracketed paste. A paste of more text than one frame
/// carries never leaves this terminal.
///
/// [`RuntimeEvent::Quit`] never reaches here: the loop reads it as
/// [`AttachmentEnding::TerminalGone`] and stops. An input thread runs only for a terminal
/// that had keys to read, so a read failure from one is that terminal going
/// away.
///
/// `cell_size_query` holds the attachment's terminal query state: a resize
/// invalidates it and a cell-size reply settles it. A fired binding's
/// viewer-local action reads `mouse_frame`, the frame this terminal last
/// painted.
///
/// Returns [`ControlFlow::Break`] when the key fired the quit binding, after
/// `core:quit` is queued, and [`ControlFlow::Continue`] for every other event.
fn process_runtime_input_with_cell_size(
    client: &mut Client,
    uplink: &mut Uplink,
    cell_size_query: &mut terminal::CellSizeQuery,
    mouse_frame: Option<&MouseFrame>,
    runtime_event: RuntimeEvent,
    read_at: Instant,
    pane_key_inputs: &mut Vec<KeyInput>,
) -> ControlFlow<()> {
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
            // A release has no binding chord. Dock/undock mode, pane placement
            // mode, the move chooser, and floating pane move mode own releases
            // while on; every other release goes to the focused pane.
            let Some(chord) = key_input.to_binding_chord() else {
                if client.find_viewer_local_input_mode().is_none() {
                    pane_key_inputs.push(key_input);
                }
                return ControlFlow::Continue(());
            };
            match client.resolve_key(chord, read_at) {
                KeyOutcome::Fire(bound_action) => {
                    send_pane_key_inputs(uplink, pane_key_inputs);
                    return uplink.submit_bound_action(client, bound_action, mouse_frame);
                }
                KeyOutcome::PassThrough(_) => {
                    // The key belongs to the program in the pane, so a
                    // selection gesture over it is over.
                    client.end_mouse_selection();
                    pane_key_inputs.push(key_input);
                }
                // Held or dropped: nothing more reaches the session. A chord
                // that opens or closes a sequence moves the breadcrumb the
                // hint bar draws. An `Esc` that ends a grab-handle drag clears
                // the breadcrumb, and the floating pane is drawn where the
                // frame places it. Any other discard draws nothing.
                KeyOutcome::Pending | KeyOutcome::Discard => {
                    send_pane_key_inputs(uplink, pane_key_inputs);
                }
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
            if client.find_viewer_local_input_mode().is_some() {
                return ControlFlow::Continue(());
            }
            // The text belongs to the program in the pane, so a selection
            // gesture over it is over.
            client.end_mouse_selection();
            uplink.send_request(IpcRequestKind::Paste { pasted_text });
        }
        _ => {}
    }
    ControlFlow::Continue(())
}

/// Run the viewer's input timers that are due at `judge_time`, in this order:
///
/// 1. An open key sequence whose ambiguity deadline passed fires. The held
///    mouse actions go up first as one round ([`flush_mouse_round`]), then the
///    commands the sequence resolves to. A viewer-local action reads
///    `mouse_frame`, the frame this terminal last painted.
/// 2. A placement tab hover that has lasted
///    [`PLACEMENT_TAB_HOVER_DELAY_DURATION`](crate::PLACEMENT_TAB_HOVER_DELAY_DURATION)
///    selects the tab under the pointer as the placement destination.
/// 3. A selection drag held past a pane's edge adds its due scroll step to
///    `pending_mouse_actions`, and its next step comes due one
///    `SELECTION_SCROLL_INTERVAL_DURATION` after `judge_time`.
///
/// A sequence that is both a complete binding and a longer one's prefix fires
/// when its deadline passes. The viewer fires it, and the session runs the
/// commands that come back.
///
/// Returns [`ControlFlow::Break`] when the sequence fired the quit binding,
/// after `core:quit` is queued, and runs no other timer then. Returns
/// [`ControlFlow::Continue`] otherwise.
fn expire_input_timers(
    client: &mut Client,
    uplink: &mut Uplink,
    mouse_frame: Option<&MouseFrame>,
    sent_border_moves: &mut Vec<SentBorderMove>,
    pending_mouse_actions: &mut Vec<MouseAction>,
    judge_time: Instant,
) -> ControlFlow<()> {
    if let Some(bound_action) = client.expire_key_sequence(judge_time) {
        flush_mouse_round(uplink, sent_border_moves, pending_mouse_actions);
        if uplink
            .submit_bound_action(client, bound_action, mouse_frame)
            .is_break()
        {
            return ControlFlow::Break(());
        }
    }
    client.expire_placement_tab_hover(judge_time);
    if let Some(mouse_frame) = mouse_frame {
        queue_mouse_actions(
            pending_mouse_actions,
            client.expire_mouse_scroll(judge_time, mouse_frame),
        );
    }
    ControlFlow::Continue(())
}

/// Answer the events of one read of this terminal, other than its hangup,
/// oldest first, against `mouse_frame`, the frame this terminal last painted.
/// Requests leave `uplink` in the order of the inputs that caused them.
///
/// The events are judged at `terminal_input.read_at`, the instant the input
/// thread read them. The loop records that it took the read
/// ([`InputReadClock::note_input_taken`]), and every input timer due at that
/// instant runs first ([`expire_input_timers`]). Example: `<C-y>` binds
/// `core:new-tab` and opens `<C-y> a`, the chord timeout is 500 ms, and a paste
/// is read 600 ms after `<C-y>`: the `NewTab` command goes up, then the paste.
///
/// The keys of the read that go to the pane go up as one
/// [`IpcRequestKind::Keyboard`]. Any event that is not a key, and any key
/// outcome that does not go to the pane, sends the keys collected before it
/// first, so the keys after it go up as a second request. Example: `ab<C-t>cd`
/// in one read, with `<C-t>` bound to `core:new-tab`, sends `Keyboard` for `a`
/// and `b`, then `NewTab`, then `Keyboard` for `c` and `d`.
///
/// Each event, in turn:
///
/// - A mouse event that pane placement takes
///   ([`Client::handle_placement_mouse`]) writes the held mouse actions as one
///   round ([`flush_mouse_round`]), then sends what placement decided. An
///   unshifted left press on a placement handle opens placement.
/// - Any other mouse event adds what the viewer decided to
///   `pending_mouse_actions` ([`handle_mouse_event`]), which the pass writes at
///   its end. Before the first paint, `mouse_frame` is `None`, and a mouse
///   event changes nothing.
/// - A loss of the outer terminal's focus ends what
///   [`Client::handle_outer_terminal_focus_lost`] names, and sends nothing of
///   its own.
/// - Every other event writes the held mouse actions as one round, then sends
///   its own requests
///   ([`process_runtime_input_with_cell_size`]).
///
/// Example: a left press on floating pane `B`, then `Tab` in floating pane
/// move mode, in one pass, write the round with `FocusPane` for `B`, then the
/// `FocusPane` that `Tab` sends.
///
/// Returns [`ControlFlow::Break`] when a key, or a sequence whose deadline
/// passed before the events were read, fired the quit binding, handling none
/// of the read's events after it, and [`ControlFlow::Continue`] otherwise.
fn handle_terminal_input_event(
    client: &mut Client,
    uplink: &mut Uplink,
    cell_size_query: &mut terminal::CellSizeQuery,
    mouse_frame: Option<&MouseFrame>,
    terminal_input: TerminalInput,
    sent_border_moves: &mut Vec<SentBorderMove>,
    pending_mouse_actions: &mut Vec<MouseAction>,
) -> ControlFlow<()> {
    let TerminalInput {
        runtime_events,
        read_at,
    } = terminal_input;
    client.get_input_read_clock().note_input_taken(read_at);
    if expire_input_timers(
        client,
        uplink,
        mouse_frame,
        sent_border_moves,
        pending_mouse_actions,
        read_at,
    )
    .is_break()
    {
        return ControlFlow::Break(());
    }
    let mut pane_key_inputs = Vec::new();
    for runtime_event in runtime_events {
        if !matches!(runtime_event, RuntimeEvent::KeyInput { .. }) {
            send_pane_key_inputs(uplink, &mut pane_key_inputs);
        }
        match runtime_event {
            RuntimeEvent::MouseInput { mouse_input, .. } => {
                let Some(mouse_frame) = mouse_frame else {
                    continue;
                };
                match client.handle_placement_mouse(mouse_input, mouse_frame, read_at) {
                    Some(placement_input_action) => {
                        flush_mouse_round(uplink, sent_border_moves, pending_mouse_actions);
                        uplink.submit_placement_input_action(client, placement_input_action);
                    }
                    None => handle_mouse_event(
                        client,
                        mouse_frame,
                        mouse_input,
                        read_at,
                        pending_mouse_actions,
                    ),
                }
            }
            RuntimeEvent::OuterTerminalFocusLost { .. } => {
                client.handle_outer_terminal_focus_lost();
            }
            other_runtime_event => {
                flush_mouse_round(uplink, sent_border_moves, pending_mouse_actions);
                if process_runtime_input_with_cell_size(
                    client,
                    uplink,
                    cell_size_query,
                    mouse_frame,
                    other_runtime_event,
                    read_at,
                    &mut pane_key_inputs,
                )
                .is_break()
                {
                    return ControlFlow::Break(());
                }
            }
        }
    }
    send_pane_key_inputs(uplink, &mut pane_key_inputs);
    ControlFlow::Continue(())
}

/// Send `pane_key_inputs`, keys of one read that go to the pane, as one
/// [`IpcRequestKind::Keyboard`] request, and leave it empty. Sends nothing
/// when it has no key.
fn send_pane_key_inputs(uplink: &mut Uplink, pane_key_inputs: &mut Vec<KeyInput>) {
    if pane_key_inputs.is_empty() {
        return;
    }
    uplink.send_request(IpcRequestKind::Keyboard {
        key_inputs: take(pane_key_inputs),
    });
}

/// Draw `render_snapshot`, a painted frame whose image records are complete,
/// at the cell size `cell_size_query` holds, and adopt the mouse frame it
/// painted ([`adopt_painted_mouse_frame`]).
fn draw_completed_painted_frame<B: Backend>(
    screen: &mut Screen<B>,
    client: &mut Client,
    cell_size_query: &terminal::CellSizeQuery,
    last_mouse_frame: &mut Option<MouseFrame>,
    render_snapshot: RenderSnapshot,
) {
    screen.set_cell_size(cell_size_query.get_current_cell_size());
    if let Some(mouse_frame) = screen.draw_snapshot(client, render_snapshot) {
        adopt_painted_mouse_frame(client, last_mouse_frame, mouse_frame);
    }
}

/// Make `mouse_frame`, the frame this terminal just painted, the frame the
/// next mouse events are placed against, after `client` drops every gesture
/// that frame ended ([`Client::drop_gestures_the_frame_ended`]).
fn adopt_painted_mouse_frame(
    client: &mut Client,
    last_mouse_frame: &mut Option<MouseFrame>,
    mouse_frame: MouseFrame,
) {
    client.drop_gestures_the_frame_ended(&mouse_frame);
    *last_mouse_frame = Some(mouse_frame);
}

/// Answer one mouse event read from this terminal against `mouse_frame`, the
/// frame this terminal last painted, and add everything the viewer decided to
/// `pending_mouse_actions` through [`queue_mouse_actions`]. `read_at` is when
/// the input thread read the event: a double click is judged against it.
///
/// Viewer state moves at once — the hovered pane, the gesture under way, the
/// capture a press takes — so a drag keeps tracking the pointer while earlier
/// rounds are still unanswered.
fn handle_mouse_event(
    client: &mut Client,
    mouse_frame: &MouseFrame,
    mouse_input: MouseInput,
    read_at: Instant,
    pending_mouse_actions: &mut Vec<MouseAction>,
) {
    queue_mouse_actions(
        pending_mouse_actions,
        client.handle_mouse(mouse_input, mouse_frame, read_at),
    );
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
/// that program reads; a [`MouseAction::Command`] runs once, a
/// [`MouseAction::Resize`] states a border's whole distance from its drag
/// anchor, and a [`MouseAction::MoveFloatingPane`] is the move a drawn draft
/// waits for. A pile holding no scrolls stays over the cap.
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

/// Take the cells the session just applied off every buffered border move for
/// `pane_id`'s `border_side`.
///
/// A buffered move names the whole distance from the drag anchor to the pointer
/// position it was decided at, and the answered move applied `applied_cell_count`
/// cells of that same distance in the direction `resize_step` names. What is
/// left to ask for is each buffered move's own signed distance minus
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
/// Nothing is waited for and nothing is timed: the pile is only ever what the
/// mouse events of one pass decided since the pass last wrote, at its start or
/// before the request of another input
/// ([`handle_terminal_input_event`]), so a link fast enough to keep the loop
/// level sends the single action that woke it and folds nothing.
///
/// A border move names the whole distance from its drag anchor, so the cells the
/// moves already on the wire asked for come off it here, and so do the cells
/// this round's own earlier moves for that same border ask for: the session
/// travels every move in a round, one after another. Each move that survives
/// that is recorded in `sent_border_moves` under the `request_id` it went out with. A move
/// left with no cells to travel is dropped after the fold: the fold sees it
/// and keeps it as the newest of the border's moves.
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
    // Past `MAX_PENDING_MOUSE_ACTION_COUNT` entries, the oldest go. A forgotten
    // move leaves its cells counted as never asked, so the next move for that
    // border asks for them again.
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
            mouse_kind,
            modifier_flags,
            pane_local_cell,
        } => WireMouseAction::Forward {
            pane_id,
            mouse_kind,
            modifier_flags,
            pane_local_cell,
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
        MouseAction::MoveFloatingPane {
            command_id,
            command_args,
        } => WireMouseAction::MoveFloatingPane {
            command_id,
            command_args,
        },
    }
}

/// Fold a pile of actions into the shortest run that means the same thing.
///
/// Only neighbors fold, so an action of another kind between two foldable ones
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
///   is the whole move. A border move also folds past the move just before it
///   when that move is for the same pane's border on the other axis, as a
///   corner drag writes them: `Right`, `Down`, `Right`, `Down` becomes the newer
///   `Right` and the newer `Down`.
/// - A forward always pushes: each report is a separate event the pane's program
///   must see.
fn coalesce_mouse_actions(mouse_actions: Vec<MouseAction>) -> Vec<MouseAction> {
    let mut folded_mouse_actions: Vec<MouseAction> = Vec::with_capacity(mouse_actions.len());
    for mouse_action in mouse_actions {
        let unfolded_mouse_action = match folded_mouse_actions.as_mut_slice() {
            [.., earlier_mouse_action, tail_mouse_action]
                if is_other_axis_border_move(tail_mouse_action, &mouse_action) =>
            {
                fold_mouse_action(earlier_mouse_action, mouse_action)
            }
            [.., tail_mouse_action] => fold_mouse_action(tail_mouse_action, mouse_action),
            [] => Some(mouse_action),
        };
        if let Some(mouse_action) = unfolded_mouse_action {
            folded_mouse_actions.push(mouse_action);
        }
    }
    folded_mouse_actions
}

/// Whether `held_mouse_action` and `next_mouse_action` are border moves of one
/// pane on different axes: a `Right` move and a `Down` move of one pane are,
/// and a `Right` move and a `Left` move of one pane are not.
fn is_other_axis_border_move(
    held_mouse_action: &MouseAction,
    next_mouse_action: &MouseAction,
) -> bool {
    match (held_mouse_action, next_mouse_action) {
        (
            MouseAction::Resize {
                pane_id,
                border_side,
                ..
            },
            MouseAction::Resize {
                pane_id: next_pane_id,
                border_side: next_border_side,
                ..
            },
        ) => {
            pane_id == next_pane_id
                && matches!(border_side, Direction::Left | Direction::Right)
                    != matches!(next_border_side, Direction::Left | Direction::Right)
        }
        _ => false,
    }
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
/// error, and so does a read on a link to a session on another machine that
/// waits longer than `remote-link-timeout-ms`.
fn classify_session_event(
    session_event: &Result<SessionEvent, IpcError>,
) -> Option<AttachmentEnding> {
    match session_event {
        Ok(SessionEvent::Detached) => Some(AttachmentEnding::Detached),
        Ok(SessionEvent::Quit) => Some(AttachmentEnding::SessionEnded),
        Ok(SessionEvent::Restarting) => Some(AttachmentEnding::Restarting),
        Ok(SessionEvent::SwitchTo {
            session_id,
            owner_user_id,
        }) => Some(AttachmentEnding::SwitchSession {
            session_id: *session_id,
            owner_user_id: *owner_user_id,
        }),
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
///
/// A viewer that pressed the quit keybinding with no link prints `left session
/// <session id> while reconnecting`, then `the session continues without you`,
/// then that same way back, and exits zero.
fn report_attachment_ending(
    home: &Home,
    ending: AttachmentEnding,
    session_id: SessionId,
    client_executable_path: Option<&Path>,
) -> Result<Option<(SessionId, Option<u32>)>, CliError> {
    match ending {
        AttachmentEnding::Detached => {
            println!("detached from session {session_id}");
            Ok(None)
        }
        AttachmentEnding::SessionEnded => {
            println!("the session ended");
            Ok(None)
        }
        AttachmentEnding::SwitchSession {
            session_id: next_session_id,
            owner_user_id,
        } => Ok(Some((next_session_id, owner_user_id))),
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
        AttachmentEnding::LeftWhileReconnecting => {
            println!(
                "left session {session_id} while reconnecting\n  the session continues without you\n  {}",
                build_reattach_instructions(home, session_id)
            );
            Ok(None)
        }
        // This ending is logged and prints nothing. The session drops this
        // client when the connection closes behind it.
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
) -> Result<Option<(SessionId, Option<u32>)>, CliError> {
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
/// A session on this machine reads ``run `koshi list-sessions`; …``. Its
/// attach command names the user id of a home that holds one, such as
/// `koshi attach 1001/session-<uuid>`. One on a server names that server in
/// both commands: `koshi attach --remote my-box` shows that server's sessions.
fn build_reattach_instructions(home: &Home, session_id: SessionId) -> String {
    match home {
        Home::Local { owner_user_id, .. } => format!(
            "run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {}`",
            format_session_target(session_id, *owner_user_id)
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
