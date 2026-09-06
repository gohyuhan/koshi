//! The outer terminal an attached client owns: the viewer built for it, the
//! thread that reads its input, and painting frames into it.
//!
//! Every item here belongs to one attached terminal. The session it is joined
//! to owns none of them.

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::attach::ViewerPaint;
use crate::{core_pane_area, Client};
use koshi_core::geometry::{PixelCellSize, Size};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::KeySequence;
use koshi_input::host::{Event, WindowSize};
use koshi_input::keyboard::decode_key;
use koshi_input::mouse::decode_mouse;
use koshi_ipc::protocol::GraphicsCapabilities;
use koshi_iterm::{
    iterm_feature_string_supports_file, iterm_feature_string_supports_sixel,
    ITERM_CAPABILITIES_QUERY,
};
use koshi_kitty::{
    write_kitty_abort, write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_placement_delete, write_kitty_support_query, KittyOutputError,
    KittyPlacement as KittyWirePlacement, KittyUpload as KittyCodecUpload, KITTY_QUERY_IMAGE_ID,
};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    cursor_position, cursor_style, image_cell_snapshot, image_paints,
    render_frame_with_image_availability, ImagePaint, ImagePlacementKey, ImageRenderMode,
    ImageSourceRect,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_sixel::{PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY};
use koshi_terminal::state::CursorShape;

use self::platform::{PlatformWaker, TerminalDevice};
use self::reader::InputReader;

mod image_output;
mod platform;
mod reader;

pub(crate) use self::image_output::{ImageOutputKind, ImageOutputState};

const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(300);

/// Request a terminal's current cell dimensions in pixels.
const CELL_SIZE_QUERY: &[u8] = b"\x1b[16t";

/// Enter the alternate screen and enable keyboard, mouse, and paste reports.
const APPLICATION_MODE_SETUP: &[u8] = b"\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h";

/// Disable paste, mouse, keyboard, and alternate-screen modes; restore cursor state.
const APPLICATION_MODE_CLEANUP: &[u8] =
    b"\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q";

/// Paints a render snapshot into ratatui's frame buffer. Every field is handed
/// straight to [`koshi_renderer::render_frame_with_images`].
pub(crate) struct SnapshotWidget<'a> {
    /// The frame the session handed out.
    pub(crate) snapshot: &'a RenderSnapshot,
    /// The colors this viewer paints koshi's chrome in.
    pub(crate) theme: &'a Theme,
    /// The hint-bar data for the mode this viewer is in.
    pub(crate) hints: &'a KeymapHints,
    /// The multi-chord sequence this viewer has open.
    pub(crate) pending: Option<&'a KeySequence>,
    /// The pane this viewer's pointer is over, and where its tab strip sits.
    pub(crate) viewer: ViewerChrome,
    /// The region solve committed with the frame being painted.
    pub(crate) committed_regions: &'a CommittedRegions,
    /// The image mode selected for this outer terminal.
    pub(crate) image_mode: ImageRenderMode,
    /// The image placements whose native bytes are ready for this frame.
    pub(crate) image_available: Option<&'a [ImagePlacementKey]>,
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_frame_with_image_availability(
            self.snapshot,
            self.committed_regions,
            self.theme,
            self.hints,
            self.pending,
            self.viewer,
            self.image_mode,
            self.image_available,
            area,
            buf,
        );
    }
}

/// The graphics capability proved by a reply from the outer terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphicsSupport {
    /// Paint image coverage with the fixed unsupported-image text.
    Unsupported,
    /// Emit Kitty raw-RGBA image commands after the text buffer is painted.
    Kitty,
    /// Advertise iTerm2 images for connection-local worker output.
    Iterm,
    /// Advertise Sixel images for connection-local worker output.
    Sixel {
        /// Number of colors available to the Sixel encoder.
        palette_colors: usize,
        /// Maximum Sixel width in pixels, or `None` when the host reports no limit.
        max_width: Option<u32>,
        /// Maximum Sixel height in pixels, or `None` when the host reports no limit.
        max_height: Option<u32>,
    },
}

/// Native Kitty image identities retained by one outer terminal connection.
pub(crate) struct KittyImageCache {
    /// Uploaded images, keyed by the connection-local content identity.
    images: HashMap<u64, KittyImage>,
    /// Terminal-side placement identities and geometry, keyed by pane and placement.
    placements: HashMap<(PaneId, u64), KittyPlacement>,
    /// The newest frame's native image placements.
    desired: Vec<ImagePaint>,
    /// The first desired placement not yet checked for an upload.
    next_upload_index: usize,
    /// The cursor to restore after placing the newest frame's images.
    cursor: Option<ratatui::layout::Position>,
    /// One compressed image transmission being advanced in bounded slices.
    upload: Option<KittyUpload>,
    /// Whether the terminal-side image cache must be cleared before reuse.
    needs_reset: bool,
    /// Whether a failed Kitty write may have left an APC transfer open.
    needs_abort: bool,
    /// The next nonzero Kitty image number.
    next_image_number: u32,
    /// The next nonzero Kitty placement identity.
    next_placement_id: u32,
}

/// One uploaded image and its outer-terminal image number.
#[derive(Clone)]
struct KittyImage {
    /// The retained record used to detect an in-process replacement.
    record: Arc<koshi_terminal::graphics::ImageRecord>,
    /// Kitty image number assigned by this client.
    image_number: u32,
}

/// One outer-terminal placement and the pixel content it displays.
#[derive(Clone, Copy, PartialEq, Eq)]
struct KittyPlacement {
    /// Kitty placement identity assigned by this client.
    id: u32,
    /// Connection-local content identity received from the session.
    content_id: u64,
    /// Destination cells in the outer terminal.
    target: Rect,
    /// Source pixels displayed inside `target`.
    source: ImageSourceRect,
    /// Horizontal pixel offset inside the first destination cell.
    cell_offset_x: Option<u32>,
    /// Vertical pixel offset inside the first destination cell.
    cell_offset_y: Option<u32>,
    /// Kitty vertical stacking order.
    z_index: i32,
}

impl KittyPlacement {
    /// Build the terminal-side placement state for one paint.
    fn new(id: u32, paint: &ImagePaint) -> Self {
        Self {
            id,
            content_id: paint.content_id,
            target: paint.target,
            source: paint.source,
            cell_offset_x: paint.cell_offset_x,
            cell_offset_y: paint.cell_offset_y,
            z_index: paint.z_index,
        }
    }

    /// Report whether the outer terminal already has this paint state.
    fn matches_paint(self, paint: &ImagePaint) -> bool {
        self.content_id == paint.content_id
            && self.target == paint.target
            && self.source == paint.source
            && self.cell_offset_x == paint.cell_offset_x
            && self.cell_offset_y == paint.cell_offset_y
            && self.z_index == paint.z_index
    }

    /// Retain this placement identity with a paint's current state.
    fn adopt_paint(&mut self, paint: &ImagePaint) {
        let id = self.id;
        *self = Self::new(id, paint);
    }
}

/// One Kitty upload advanced between attachment-loop passes.
struct KittyUpload {
    /// Connection-local content identity received from the session.
    content_id: u64,
    /// Record retained while its compressed bytes are generated and sent.
    record: Arc<koshi_terminal::graphics::ImageRecord>,
    /// The bounded Kitty upload encoder.
    codec: KittyCodecUpload,
}

impl Default for KittyImageCache {
    fn default() -> Self {
        Self {
            images: HashMap::new(),
            placements: HashMap::new(),
            desired: Vec::new(),
            next_upload_index: 0,
            cursor: None,
            upload: None,
            needs_reset: false,
            needs_abort: false,
            next_image_number: 1,
            next_placement_id: 1,
        }
    }
}

/// A failure while painting a frame or emitting its native image data.
#[derive(Debug)]
pub(crate) enum PaintError<E> {
    /// The ratatui backend did not accept the frame buffer.
    Backend(E),
    /// The native image writer did not accept its output.
    Image(io::Error),
}

impl GraphicsSupport {
    /// Map the terminal capability to the renderer's image mode.
    pub(crate) fn image_mode(self) -> ImageRenderMode {
        match self {
            Self::Unsupported => ImageRenderMode::Placeholder,
            Self::Kitty => ImageRenderMode::Native,
            Self::Iterm | Self::Sixel { .. } => ImageRenderMode::Native,
        }
    }

    /// Convert the selected connection-local protocol to its attach report.
    pub(crate) const fn capabilities(self) -> GraphicsCapabilities {
        match self {
            Self::Unsupported => GraphicsCapabilities {
                kitty: false,
                iterm: false,
                sixel: false,
            },
            Self::Kitty => GraphicsCapabilities {
                kitty: true,
                iterm: false,
                sixel: false,
            },
            Self::Iterm => GraphicsCapabilities {
                kitty: false,
                iterm: true,
                sixel: false,
            },
            Self::Sixel { .. } => GraphicsCapabilities {
                kitty: false,
                iterm: false,
                sixel: true,
            },
        }
    }
}

/// The values gathered by one bounded terminal capability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalProbe {
    /// The preferred protocol proved by the terminal.
    graphics: GraphicsSupport,
    /// The cell dimensions reported by the terminal, if valid.
    cell_size: Option<PixelCellSize>,
    /// Whether the probe's CSI 16t request still awaits an outer-terminal reply.
    cell_size_query_pending: bool,
}

impl TerminalProbe {
    /// Build the result for a terminal that answered no capability query.
    const fn unsupported() -> Self {
        Self {
            graphics: GraphicsSupport::Unsupported,
            cell_size: None,
            cell_size_query_pending: false,
        }
    }
}

/// Coordinates cell-size requests with resize reports on one terminal.
#[derive(Debug)]
pub(crate) struct CellSizeQuery {
    /// The most recent measurement accepted for the current viewport.
    current: Option<PixelCellSize>,
    /// Whether a CSI 16t request has not received its reply.
    pending: bool,
    /// Whether the pending reply belongs to an older viewport.
    discard_pending: bool,
    /// Whether this attachment can write terminal queries.
    enabled: bool,
}

impl CellSizeQuery {
    /// Build a coordinator with the measurement captured before Attach and the
    /// probe query's outstanding state.
    pub(crate) fn new(current: Option<PixelCellSize>, enabled: bool, pending: bool) -> Self {
        Self {
            current,
            pending: enabled && pending,
            discard_pending: false,
            enabled,
        }
    }

    /// Return the measurement to place in the next Attach.
    pub(crate) fn current(&self) -> Option<PixelCellSize> {
        self.current
    }

    /// Invalidate or replace the measurement for a resized viewport.
    ///
    /// A pending reply is discarded because CSI 16t carries no request id. A
    /// fresh request is needed only when the resize has no usable local metric.
    pub(crate) fn resize(&mut self, current: Option<PixelCellSize>) -> bool {
        self.current = current;
        if self.pending {
            self.discard_pending = true;
            false
        } else {
            current.is_none()
        }
    }

    /// Accept a reply, returning the value to send to the session and whether
    /// a fresh query must be written after discarding an older reply.
    pub(crate) fn accept(&mut self, size: PixelCellSize) -> (Option<PixelCellSize>, bool) {
        if !self.pending {
            return (None, false);
        }
        self.pending = false;
        if self.discard_pending {
            self.discard_pending = false;
            return (None, self.current.is_none());
        }
        self.current = Some(size);
        (Some(size), false)
    }

    /// Write one CSI 16t request at the attachment loop's output boundary.
    pub(crate) fn request(&mut self) {
        if !self.enabled || self.pending {
            return;
        }
        let mut writer = io::stdout().lock();
        if let Err(error) = self.request_to(&mut writer) {
            tracing::warn!(%error, "could not request terminal cell dimensions");
        }
    }

    /// Write one request through `writer`, retaining pending state after an
    /// attempted write until a matching ordered reply is consumed.
    fn request_to<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if !self.enabled || self.pending || self.current.is_some() {
            return Ok(());
        }
        self.pending = true;
        writer.write_all(CELL_SIZE_QUERY)?;
        writer.flush()
    }
}

/// The input, mode, and capability-query owner for one attached terminal.
pub(crate) struct TerminalOwner {
    /// Native graphics support proved by the terminal's protocol answer.
    graphics: GraphicsSupport,
    /// Initial pixel dimensions of one terminal cell, if the probe reported them.
    cell_size: Option<PixelCellSize>,
    /// Whether the initial CSI 16t query still awaits the outer terminal.
    cell_size_query_pending: bool,
    /// Whether standard output is the terminal receiving rendered frames.
    output_is_terminal: bool,
    /// Terminal handle used for protocol output and platform-mode restoration.
    terminal: Arc<Mutex<Option<TerminalDevice>>>,
    /// Parsed input source shared with the input thread. `None` when both
    /// standard streams are redirected and no terminal work is needed.
    reader: Option<InputReader>,
    /// Wakes the input reader when this attachment ends. It is present exactly
    /// when `reader` is present.
    waker: Option<PlatformWaker>,
    /// Stops input delivery before terminal restoration.
    shutdown: Arc<AtomicBool>,
    /// Whether this attachment enabled application-level terminal modes.
    application_modes_active: Arc<AtomicBool>,
    /// Ensures one path cancels Kitty transfers and deletes this attachment's images.
    image_cleanup_claimed: Arc<AtomicBool>,
    /// The input thread started after Attach succeeds.
    input_thread: Option<thread::JoinHandle<()>>,
    /// Rejects a second activation of the same terminal.
    activated: bool,
}

impl TerminalOwner {
    /// Open the controlling terminal and probe its capabilities.
    /// With piped input and output, build an unsupported owner without opening
    /// `/dev/tty`; that client reads no keys and writes its frame to the pipe.
    pub(crate) fn start() -> Result<Self, String> {
        let input_is_terminal = io::stdin().is_terminal();
        let output_is_terminal = io::stdout().is_terminal();
        let (graphics, cell_size, cell_size_query_pending, terminal, reader, waker) =
            if terminal_device_needed(input_is_terminal, output_is_terminal) {
                let (mut terminal, source) = TerminalDevice::open()
                    .map_err(|error| format!("could not open the terminal: {error}"))?;
                let mut reader = InputReader::new(source);
                let probe = graphics_support_for_output(output_is_terminal, || {
                    with_raw_mode(
                        &mut terminal,
                        |terminal| terminal.enter_raw_mode(),
                        |terminal| probe_terminal(terminal, &mut reader),
                        |terminal| terminal.enter_cooked_mode(),
                    )
                })
                .map_err(|error| format!("could not probe terminal graphics support: {error}"))?;
                let waker = reader.waker();
                (
                    probe.graphics,
                    probe.cell_size,
                    probe.cell_size_query_pending,
                    Some(terminal),
                    Some(reader),
                    Some(waker),
                )
            } else {
                (GraphicsSupport::Unsupported, None, false, None, None, None)
            };
        let image_cleanup_claimed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            graphics,
            cell_size,
            cell_size_query_pending,
            output_is_terminal,
            terminal: Arc::new(Mutex::new(terminal)),
            reader,
            waker,
            shutdown: Arc::new(AtomicBool::new(false)),
            application_modes_active: Arc::new(AtomicBool::new(false)),
            image_cleanup_claimed,
            input_thread: None,
            activated: false,
        })
    }

    /// Return the native graphics support proved by the terminal probe.
    pub(crate) fn graphics(&self) -> GraphicsSupport {
        self.graphics
    }

    /// Return the cell dimensions captured during the initial terminal probe.
    #[allow(dead_code)]
    pub(crate) fn cell_size(&self) -> Option<PixelCellSize> {
        self.cell_size
    }

    /// Build the cell-size coordinator used after this terminal attaches.
    pub(crate) fn cell_size_query(&self) -> CellSizeQuery {
        CellSizeQuery::new(
            self.cell_size,
            self.output_is_terminal,
            self.cell_size_query_pending,
        )
    }

    /// Register panic-safe image cleanup and terminal restoration.
    pub(crate) fn register_restore(&self, cleanup: &TerminalCleanupGuard) {
        let terminal = Arc::clone(&self.terminal);
        let graphics = self.graphics;
        let shutdown = Arc::clone(&self.shutdown);
        let waker = self.waker.clone();
        let application_modes_active = Arc::clone(&self.application_modes_active);
        let image_cleanup_claimed = Arc::clone(&self.image_cleanup_claimed);
        cleanup.register_cleanup(Box::new(move || {
            shutdown.store(true, Ordering::Release);
            if let Some(waker) = &waker {
                let _ = waker.wake();
            }
            try_restore_shared_terminal(
                &terminal,
                graphics,
                &application_modes_active,
                &image_cleanup_claimed,
            );
        }));
    }

    /// Enable terminal modes and start input delivery after Attach succeeds.
    pub(crate) fn activate(
        &mut self,
        inbox_tx: mpsc::SyncSender<RuntimeEvent>,
        client_id: ClientId,
        read_input: bool,
    ) -> Result<(), String> {
        if self.activated {
            return Err("terminal owner was already activated".to_string());
        }
        self.activated = true;
        {
            let mut guard = lock_terminal(&self.terminal);
            if let Some(terminal) = guard.as_mut() {
                terminal
                    .enter_raw_mode()
                    .map_err(|error| format!("could not enter terminal raw mode: {error}"))?;
                if self.output_is_terminal {
                    self.application_modes_active.store(true, Ordering::Release);
                    enable_terminal_modes(terminal, self.graphics)
                        .map_err(|error| format!("could not enable terminal modes: {error}"))?;
                }
            } else if self.output_is_terminal || read_input {
                return Err("terminal owner was already restored".to_string());
            }
        }
        if !read_input {
            return Ok(());
        }

        let mut reader = self
            .reader
            .take()
            .ok_or_else(|| "terminal input reader is unavailable".to_string())?;
        let shutdown = Arc::clone(&self.shutdown);
        let panic_tx = inbox_tx.clone();
        self.input_thread = Some(
            thread::Builder::new()
                .name("koshi-terminal-input".to_string())
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        run_terminal_input(&mut reader, &inbox_tx, client_id, &shutdown);
                    }));
                    if result.is_err() {
                        let _ = panic_tx.send(RuntimeEvent::Quit);
                    }
                })
                .map_err(|error| format!("could not spawn the terminal input thread: {error}"))?,
        );
        Ok(())
    }

    /// Stop input delivery and restore the host terminal state.
    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    /// Signal and join the input thread, then restore every terminal mode.
    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(waker) = &self.waker {
            let _ = waker.wake();
        }
        if let Some(thread) = self.input_thread.take() {
            let _ = thread.join();
        }
        restore_shared_terminal(
            &self.terminal,
            self.graphics,
            &self.application_modes_active,
            &self.image_cleanup_claimed,
        );
    }
}

impl Drop for TerminalOwner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether input or output needs access to the controlling terminal.
fn terminal_device_needed(input_is_terminal: bool, output_is_terminal: bool) -> bool {
    input_is_terminal || output_is_terminal
}

/// Probe only when standard output is the terminal that will receive images.
fn graphics_support_for_output(
    output_is_terminal: bool,
    probe: impl FnOnce() -> io::Result<TerminalProbe>,
) -> io::Result<TerminalProbe> {
    if output_is_terminal {
        probe()
    } else {
        Ok(TerminalProbe::unsupported())
    }
}

/// Run one terminal operation between raw-mode entry and cooked-mode restore.
fn with_raw_mode<T, R>(
    terminal: &mut T,
    enter_raw: impl FnOnce(&mut T) -> io::Result<()>,
    operation: impl FnOnce(&mut T) -> io::Result<R>,
    enter_cooked: impl FnOnce(&mut T) -> io::Result<()>,
) -> io::Result<R> {
    enter_raw(terminal)?;
    let result = operation(terminal);
    let restored = enter_cooked(terminal);
    match (result, restored) {
        (Err(error), Err(restore_error)) => {
            tracing::warn!(%restore_error, "could not restore terminal after failed operation");
            Err(error)
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

/// Lock the shared terminal and recover its value after a poisoned lock.
fn lock_terminal(
    terminal: &Mutex<Option<TerminalDevice>>,
) -> std::sync::MutexGuard<'_, Option<TerminalDevice>> {
    terminal
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restore the shared terminal once, waiting for an in-progress terminal write.
fn restore_shared_terminal(
    shared: &Mutex<Option<TerminalDevice>>,
    graphics: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let terminal = lock_terminal(shared).take();
    let Some(mut terminal) = terminal else {
        return;
    };
    restore_terminal(
        &mut terminal,
        graphics,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore without blocking a panic hook on the thread that holds the terminal.
fn try_restore_shared_terminal(
    shared: &Mutex<Option<TerminalDevice>>,
    graphics: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut guard = match shared.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            write_fallback_terminal_cleanup(
                graphics,
                application_modes_active,
                image_cleanup_claimed,
            );
            return;
        }
    };
    let Some(mut terminal) = guard.take() else {
        return;
    };
    drop(guard);
    restore_terminal(
        &mut terminal,
        graphics,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore application modes and the platform's cooked terminal mode.
fn restore_terminal(
    terminal: &mut TerminalDevice,
    graphics: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    if let Err(error) = restore_application_modes(
        terminal,
        graphics,
        application_modes_active,
        image_cleanup_claimed,
    ) {
        tracing::warn!(%error, "could not restore terminal application modes");
    }
    if let Err(error) = terminal.enter_cooked_mode() {
        tracing::warn!(%error, "could not restore terminal cooked mode");
    }
}

/// Claim and restore this attachment's application-level terminal modes once.
fn restore_application_modes<W: Write>(
    writer: &mut W,
    graphics: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    if !application_modes_active.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    write_terminal_cleanup(writer, graphics, image_cleanup_claimed)
}

/// Claim the one Kitty cleanup allowed for an attachment.
fn claim_image_cleanup(claimed: &AtomicBool) -> bool {
    !claimed.swap(true, Ordering::AcqRel)
}

/// Gather every capability reply available before one shared deadline.
fn probe_terminal<S: reader::EventSource, W: Write>(
    writer: &mut W,
    reader: &mut InputReader<S>,
) -> io::Result<TerminalProbe> {
    write_terminal_probe_queries(writer)?;
    let deadline = Instant::now() + TERMINAL_QUERY_TIMEOUT;
    let mut replies = ProbeReplies::default();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !reader.poll(Some(remaining), is_probe_event)? {
            break;
        }
        let event = reader.read(is_probe_event)?;
        replies.observe(event);
        if Instant::now() >= deadline {
            break;
        }
    }
    Ok(replies.finish())
}

/// Replies collected while capability queries share one deadline.
#[derive(Default)]
struct ProbeReplies {
    kitty: bool,
    iterm_file: bool,
    iterm_sixel: bool,
    da1_sixel: bool,
    palette: Option<Result<u32, koshi_input::host::GraphicAttributeError>>,
    geometry: Option<Result<(u32, u32), koshi_input::host::GraphicAttributeError>>,
    cell_size: Option<PixelCellSize>,
}

impl ProbeReplies {
    /// Retain one parsed event that belongs to the capability probe.
    fn observe(&mut self, event: Event) {
        match event {
            Event::KittyGraphicsReply(reply) if reply.image_id == KITTY_QUERY_IMAGE_ID => {
                self.kitty |= reply.ok;
            }
            Event::TerminalFeatures(features) => {
                self.iterm_file |= iterm_feature_string_supports_file(&features);
                self.iterm_sixel |= iterm_feature_string_supports_sixel(&features);
            }
            Event::PrimaryDeviceAttributes(attributes) => {
                self.da1_sixel |= attributes
                    .get(1..)
                    .is_some_and(|features| features.contains(&4));
            }
            Event::SixelGraphicsAttributeReply(reply) => match reply {
                koshi_input::host::GraphicAttributeReply::Palette(value) => {
                    if self.palette.is_none() {
                        self.palette = Some(value);
                    }
                }
                koshi_input::host::GraphicAttributeReply::Geometry(value) => {
                    if self.geometry.is_none() {
                        self.geometry = Some(value);
                    }
                }
            },
            Event::CellSize(size) => {
                if self.cell_size.is_none() {
                    self.cell_size = Some(size);
                }
            }
            Event::KittyGraphicsReply(_)
            | Event::Key(_)
            | Event::Mouse(_)
            | Event::WindowResized(_)
            | Event::Paste(_)
            | Event::FocusIn
            | Event::FocusOut => {}
        }
    }

    /// Select the preferred protocol and retain the measured cell size.
    fn finish(self) -> TerminalProbe {
        let graphics = if self.kitty {
            GraphicsSupport::Kitty
        } else if self.iterm_file {
            GraphicsSupport::Iterm
        } else {
            self.sixel_support().unwrap_or(GraphicsSupport::Unsupported)
        };
        TerminalProbe {
            graphics,
            cell_size: self.cell_size,
            cell_size_query_pending: self.cell_size.is_none(),
        }
    }

    /// Build bounded Sixel support from the terminal's positive evidence.
    fn sixel_support(&self) -> Option<GraphicsSupport> {
        let advertised = self.da1_sixel || self.iterm_sixel || matches!(self.geometry, Some(Ok(_)));
        if !advertised {
            return None;
        }
        let palette_colors = match self.palette {
            Some(Ok(value)) if value < 2 => return None,
            Some(Ok(value)) => value.min(256) as usize,
            Some(Err(_)) | None => 2,
        };
        let (max_width, max_height) = match self.geometry {
            Some(Ok((width, height))) => (
                (width != 0).then_some(width),
                (height != 0).then_some(height),
            ),
            Some(Err(_)) | None => (None, None),
        };
        Some(GraphicsSupport::Sixel {
            palette_colors,
            max_width,
            max_height,
        })
    }
}

/// Write every bounded capability query and flush them as one probe batch.
fn write_terminal_probe_queries<W: Write>(writer: &mut W) -> io::Result<()> {
    write_kitty_support_query(writer).map_err(kitty_output_error)?;
    writer.write_all(ITERM_CAPABILITIES_QUERY)?;
    writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    writer.write_all(SIXEL_PALETTE_QUERY)?;
    writer.write_all(SIXEL_GEOMETRY_QUERY)?;
    writer.write_all(CELL_SIZE_QUERY)?;
    writer.flush()
}

/// Return whether an event belongs to the capability probe.
fn is_probe_event(event: &Event) -> bool {
    matches!(event, Event::KittyGraphicsReply(reply) if reply.image_id == KITTY_QUERY_IMAGE_ID)
        || matches!(event, Event::TerminalFeatures(_))
        || matches!(event, Event::PrimaryDeviceAttributes(_))
        || matches!(event, Event::SixelGraphicsAttributeReply(_))
        || matches!(event, Event::CellSize(_))
}

/// Request the terminal modes used by the attached viewer.
fn enable_terminal_modes<W: Write>(writer: &mut W, graphics: GraphicsSupport) -> io::Result<()> {
    if matches!(graphics, GraphicsSupport::Sixel { .. }) {
        writer.write_all(image_output::sixel_mode_save())?;
    }
    writer.write_all(APPLICATION_MODE_SETUP)?;
    writer.flush()
}

/// Write every application-level terminal reset in reverse setup order.
fn write_terminal_cleanup<W: Write>(
    writer: &mut W,
    graphics: GraphicsSupport,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    let mut first_error = None;
    if matches!(graphics, GraphicsSupport::Kitty) && claim_image_cleanup(image_cleanup_claimed) {
        retain_first_io_error(
            &mut first_error,
            write_kitty_abort(writer).map_err(kitty_output_error),
        );
        retain_first_io_error(
            &mut first_error,
            write_kitty_delete_all(writer).map_err(kitty_output_error),
        );
    }
    if matches!(graphics, GraphicsSupport::Sixel { .. }) {
        retain_first_io_error(&mut first_error, image_output::write_image_abort(writer));
        retain_first_io_error(
            &mut first_error,
            writer.write_all(image_output::sixel_mode_restore()),
        );
    }
    retain_first_io_error(&mut first_error, writer.write_all(APPLICATION_MODE_CLEANUP));
    retain_first_io_error(&mut first_error, writer.flush());
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Keep the first terminal I/O failure while cleanup attempts every reset.
fn retain_first_io_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

/// Write application resets when the panic hook cannot take the terminal lock.
fn write_fallback_terminal_cleanup(
    graphics: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut stdout = io::stdout();
    let _ = restore_application_modes(
        &mut stdout,
        graphics,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Read semantic terminal events until shutdown or input failure.
fn run_terminal_input(
    reader: &mut InputReader,
    inbox_tx: &mpsc::SyncSender<RuntimeEvent>,
    client_id: ClientId,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Acquire) {
        let runtime_event = match reader.read(|_| true) {
            Ok(event) => terminal_runtime_event(client_id, event),
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted
                    && shutdown.load(Ordering::Acquire) =>
            {
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::warn!(%error, "could not read terminal input");
                Some(RuntimeEvent::Quit)
            }
        };
        if let Some(runtime_event) = runtime_event {
            let quit = matches!(runtime_event, RuntimeEvent::Quit);
            if inbox_tx.send(runtime_event).is_err() || quit {
                break;
            }
        }
    }
}

/// Convert one host-terminal event into the runtime event the viewer consumes.
fn terminal_runtime_event(client_id: ClientId, event: Event) -> Option<RuntimeEvent> {
    match event {
        Event::Key(key) => decode_key(key).map(|chord| RuntimeEvent::KeyInput { client_id, chord }),
        Event::CellSize(size) => Some(RuntimeEvent::CellSize { client_id, size }),
        Event::WindowResized(resize) => Some(resize_runtime_event(client_id, resize)),
        Event::Mouse(mouse) => Some(RuntimeEvent::MouseInput {
            client_id,
            mouse: decode_mouse(mouse),
        }),
        Event::Paste(text) => Some(RuntimeEvent::HostPaste { client_id, text }),
        Event::FocusIn
        | Event::FocusOut
        | Event::PrimaryDeviceAttributes(_)
        | Event::TerminalFeatures(_)
        | Event::SixelGraphicsAttributeReply(_)
        | Event::KittyGraphicsReply(_) => None,
    }
}

/// Build the runtime resize event for one host size report.
fn resize_runtime_event(client_id: ClientId, resize: WindowSize) -> RuntimeEvent {
    let size = Size {
        cols: resize.cols,
        rows: resize.rows,
    };
    RuntimeEvent::Resize {
        client_id,
        size,
        pane_area: Some(core_pane_area(size)),
        cell_size: pixel_cell_size_from_window(resize),
    }
}

/// Derive one cell's pixel dimensions from a Unix window report only when the
/// complete pixel dimensions divide evenly across the reported grid.
fn pixel_cell_size_from_window(resize: WindowSize) -> Option<PixelCellSize> {
    let pixel_width = resize.pixel_width?;
    let pixel_height = resize.pixel_height?;
    if resize.cols == 0
        || resize.rows == 0
        || pixel_width % resize.cols != 0
        || pixel_height % resize.rows != 0
    {
        return None;
    }
    PixelCellSize::new(pixel_width / resize.cols, pixel_height / resize.rows)
}

/// Build the viewer half and apply `loaded`'s viewer-owned files, in one step.
///
/// `client_id` is the id this viewer's input events and commands carry.
/// `viewport` is this terminal's size in cells. `events` is the frame feed; a
/// client owns no session, so the receiver it is handed has no sender and its
/// frames arrive over the connection instead. `cleanup` is the guard that
/// restores the outer terminal.
///
/// `loaded.app` and `loaded.theme` fold into the viewer's settings and chrome
/// colors and always apply. `loaded.keybindings` is validated: a verdict other
/// than [`Apply`](koshi_config::conflict::KeymapVerdict::Apply) logs a warning
/// naming `koshi keys conflicts`, an `Apply` logs `"keybinding.kdl applied"`,
/// and a `None` keymap layer logs nothing.
pub(crate) fn viewer(
    client_id: ClientId,
    viewport: Size,
    events: mpsc::Receiver<koshi_renderer::snapshot::Delivery>,
    cleanup: TerminalCleanupGuard,
    loaded: koshi_link::config::LoadedConfig,
) -> Client {
    let mut client = Client::new(client_id, viewport, events, cleanup);
    match client.load_startup_config(loaded.app, loaded.theme, loaded.keybindings) {
        Some(report) if report.verdict() != koshi_config::conflict::KeymapVerdict::Apply => {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }
    client
}

/// Draw `snapshot` into `terminal`, keeping the outer terminal's window title
/// and cursor style in step with the focused pane.
///
/// The theme comes from `client`, and so does the hint bar, built for
/// `frame_paint.mode` and `frame_paint.mouse_select`. The hovered pane, the
/// tab strip's position and the open key sequence come from `frame_paint`.
/// `committed_regions` is the geometry shared by the painter and cursor
/// placement for this frame.
///
/// `last_title` and `last_cursor` store the title and cursor style used to
/// decide whether the next frame needs a control write. Both are read before
/// the buffer paint and updated after it succeeds: a changed title writes
/// `SetTitle`, and a changed cursor style writes `SetCursorStyle`. A frame
/// that [`cursor_style`] names no style stores `None` and writes no style
/// command.
///
/// # Errors
///
/// Returns a backend or native-image error when the frame cannot be fully
/// painted. A failed title or cursor-style write is ignored.
#[cfg(test)]
pub(crate) fn paint_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let mut images = KittyImageCache::default();
    let mut output = ImageOutputState::disabled();
    paint_frame_with_images(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        &mut images,
        &mut output,
        None,
        last_title,
        last_cursor,
    )
}

/// Paint one frame and schedule native Kitty images when the outer terminal
/// supports them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frame_with_images<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    images: &mut KittyImageCache,
    output: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let mut stdout = io::stdout();
    paint_frame_with_writer(
        &mut stdout,
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        image_mode,
        images,
        output,
        cell_size,
        last_title,
        last_cursor,
    )
}

/// Paint one frame and send terminal-control output to `writer`.
#[allow(clippy::too_many_arguments)]
fn paint_frame_with_writer<B: Backend, W: Write>(
    writer: &mut W,
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    images: &mut KittyImageCache,
    output: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let title = window_title(snapshot);
    let title_changed = title != *last_title;
    let cursor = cursor_style(snapshot);
    let cursor_changed = cursor != *last_cursor;
    let hints = client.frame_hints_for(frame_paint.mode, frame_paint.mouse_select);
    let size = terminal.size().map_err(PaintError::Backend)?;
    let mut paint_area = Rect::new(0, 0, size.width, size.height);
    let mut hardware_cursor = None;
    let native_output = output.kind().is_some();
    let paints = image_paints(snapshot, committed_regions, paint_area);
    let cells = native_output
        .then(|| image_cell_snapshot(snapshot, committed_regions, paint_area).map(Arc::new))
        .flatten();
    if native_output {
        output.submit_frame(&paints, cells, cell_size);
        output.poll();
        if output.screen_reset_needed() {
            terminal.clear().map_err(PaintError::Backend)?;
        }
    }
    let image_available = native_output.then(|| output.prepared_keys());
    terminal
        .draw(|frame| {
            let area = frame.area();
            paint_area = area;
            hardware_cursor = cursor_position(snapshot, committed_regions, area);
            frame.render_widget(
                SnapshotWidget {
                    snapshot,
                    theme: client.theme(),
                    hints: &hints,
                    pending: frame_paint.pending.as_ref(),
                    viewer: frame_paint.chrome,
                    committed_regions,
                    image_mode,
                    image_available,
                },
                area,
            );
            if let Some(position) = hardware_cursor {
                frame.set_cursor_position(position);
            }
        })
        .map_err(PaintError::Backend)?;
    output.mark_base_painted();
    if title_changed {
        let _ = execute!(writer, SetTitle(&title));
        *last_title = title;
    }
    if cursor_changed {
        if let Some(style) = cursor.map(set_cursor_style) {
            let _ = execute!(writer, style);
        }
        *last_cursor = cursor;
    }
    if image_mode == ImageRenderMode::Native && !native_output {
        write_kitty_frame(writer, images, &paints, hardware_cursor).map_err(PaintError::Image)?;
    }
    Ok(())
}

/// Reconcile the newest Kitty image frame and start its first missing upload.
fn write_kitty_frame<W: Write>(
    writer: &mut W,
    images: &mut KittyImageCache,
    paints: &[ImagePaint],
    cursor: Option<ratatui::layout::Position>,
) -> io::Result<()> {
    validate_image_paints(paints)?;
    images.desired = paints.to_vec();
    images.next_upload_index = 0;
    images.cursor = cursor;
    let stale_upload = images
        .upload
        .as_ref()
        .filter(|upload| !upload_is_desired(upload, &images.desired))
        .map(|upload| upload.codec.started());
    match stale_upload {
        Some(true) => mark_kitty_cache_uncertain(images),
        Some(false) => images.upload = None,
        None => {}
    }
    reconcile_kitty_frame(writer, images)?;
    start_next_kitty_upload(images)
}

/// Report whether compressed pixels still need client-loop work.
pub(crate) fn kitty_image_work_pending(images: &KittyImageCache) -> bool {
    images.upload.is_some()
}

/// Advance one bounded slice of the current Kitty upload.
pub(crate) fn advance_kitty_image<W: Write>(
    writer: &mut W,
    images: &mut KittyImageCache,
) -> io::Result<()> {
    let Some(upload) = images.upload.as_ref() else {
        return Ok(());
    };
    if !upload.codec.started() && !upload_is_desired(upload, &images.desired) {
        images.upload = None;
        reconcile_kitty_frame(writer, images)?;
        return start_next_kitty_upload(images);
    }

    let result = advance_kitty_upload(writer, images.upload.as_mut().expect("upload exists"));
    if let Err(error) = result {
        images.needs_abort = true;
        mark_kitty_cache_uncertain(images);
        return Err(error);
    }
    let complete = images
        .upload
        .as_ref()
        .is_some_and(|upload| upload.codec.complete());
    if !complete {
        return Ok(());
    }

    let upload = images.upload.take().expect("complete upload exists");
    images.images.insert(
        upload.content_id,
        KittyImage {
            record: upload.record,
            image_number: upload.codec.image_number(),
        },
    );
    place_completed_kitty_image(writer, images, upload.content_id)?;
    start_next_kitty_upload(images)
}

/// Reconcile uploaded images and placements with the newest frame.
fn reconcile_kitty_frame<W: Write>(writer: &mut W, images: &mut KittyImageCache) -> io::Result<()> {
    let mut staged_images = images.images.clone();
    let mut staged_placements = images.placements.clone();
    let mut next_image_number = images.next_image_number;
    let mut next_placement_id = images.next_placement_id;
    let mut output = Vec::new();
    let mut cursor_moved = false;
    let reset = images.needs_reset
        || image_ids_need_reset(
            &staged_images,
            &staged_placements,
            next_image_number,
            next_placement_id,
            &images.desired,
        );
    if reset {
        if images.needs_abort {
            write_kitty_abort(&mut output).map_err(kitty_output_error)?;
        }
        write_kitty_delete_all(&mut output).map_err(kitty_output_error)?;
        staged_images.clear();
        staged_placements.clear();
        next_image_number = 1;
        next_placement_id = 1;
    }

    let desired_by_key: HashMap<(PaneId, u64), &ImagePaint> = images
        .desired
        .iter()
        .map(|paint| ((paint.pane_id, paint.placement_id), paint))
        .collect();
    let desired_by_content: HashMap<u64, &ImagePaint> = images
        .desired
        .iter()
        .map(|paint| (paint.content_id, paint))
        .collect();
    let removed_images: Vec<(u64, u32)> = staged_images
        .iter()
        .filter(|(content_id, image)| {
            desired_by_content
                .get(content_id)
                .is_none_or(|paint| !kitty_image_matches(image, &paint.record))
        })
        .map(|(content_id, image)| (*content_id, image.image_number))
        .collect();
    for (content_id, image_number) in removed_images {
        write_kitty_image_delete(&mut output, image_number).map_err(kitty_output_error)?;
        staged_images.remove(&content_id);
    }
    let removed_placements: Vec<((PaneId, u64), KittyPlacement)> = staged_placements
        .iter()
        .filter(|(key, placement)| {
            desired_by_key
                .get(*key)
                .is_none_or(|paint| paint.content_id != placement.content_id)
        })
        .map(|(key, placement)| (*key, *placement))
        .collect();
    for (key, placement) in removed_placements {
        if let Some(image) = staged_images.get(&placement.content_id) {
            write_kitty_placement_delete(&mut output, image.image_number, placement.id)
                .map_err(kitty_output_error)?;
        }
        if !desired_by_key.contains_key(&key) {
            staged_placements.remove(&key);
        }
    }

    for paint in &images.desired {
        let key = (paint.pane_id, paint.placement_id);
        let placement_changed = match staged_placements.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                let id = take_nonzero_id(
                    &mut next_placement_id,
                    "Kitty placement identities are exhausted",
                )?;
                entry.insert(KittyPlacement::new(id, paint));
                true
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().matches_paint(paint) {
                    false
                } else {
                    entry.get_mut().adopt_paint(paint);
                    true
                }
            }
        };
        if !placement_changed {
            continue;
        }
        let Some(image) = staged_images
            .get(&paint.content_id)
            .filter(|image| kitty_image_matches(image, &paint.record))
        else {
            continue;
        };
        let placement_id = staged_placements[&key].id;
        write!(
            output,
            "\x1b[{};{}H",
            u32::from(paint.target.y) + 1,
            u32::from(paint.target.x) + 1
        )?;
        let placement = kitty_wire_placement(image.image_number, placement_id, paint);
        write_kitty_placement(&mut output, &image.record.image, &placement)
            .map_err(kitty_output_error)?;
        cursor_moved = true;
    }

    let abort_upload = !output.is_empty()
        && images
            .upload
            .as_ref()
            .is_some_and(|upload| upload.codec.started());
    if abort_upload && !reset {
        let mut framed = Vec::with_capacity(output.len() + 40);
        write_kitty_image_delete(
            &mut framed,
            images
                .upload
                .as_ref()
                .expect("a started upload is present")
                .codec
                .image_number(),
        )
        .map_err(kitty_output_error)?;
        framed.extend_from_slice(&output);
        output = framed;
    }
    if !output.is_empty() {
        if cursor_moved {
            restore_cursor_state(&mut output, images.cursor)?;
        }
        if let Err(error) = writer.write_all(&output).and_then(|()| writer.flush()) {
            images.needs_abort = true;
            mark_kitty_cache_uncertain(images);
            return Err(error);
        }
    }

    images.images = staged_images;
    images.placements = staged_placements;
    images.next_image_number = next_image_number;
    images.next_placement_id = next_placement_id;
    images.needs_reset = false;
    images.needs_abort = false;
    if reset || abort_upload {
        images.upload = None;
        images.next_upload_index = 0;
    }
    Ok(())
}

/// Place every use of an image that completed after this frame was reconciled.
fn place_completed_kitty_image<W: Write>(
    writer: &mut W,
    images: &mut KittyImageCache,
    content_id: u64,
) -> io::Result<()> {
    let image = images
        .images
        .get(&content_id)
        .ok_or_else(|| invalid_image_data("completed image does not match its desired frame"))?;
    let mut output = Vec::new();
    let mut placed = false;
    for paint in images
        .desired
        .iter()
        .filter(|paint| paint.content_id == content_id && kitty_image_matches(image, &paint.record))
    {
        let key = (paint.pane_id, paint.placement_id);
        let placement = images
            .placements
            .get(&key)
            .filter(|placement| placement.content_id == content_id)
            .ok_or_else(|| invalid_image_data("completed image has no Kitty placement identity"))?;
        write!(
            output,
            "\x1b[{};{}H",
            u32::from(paint.target.y) + 1,
            u32::from(paint.target.x) + 1
        )?;
        let placement = kitty_wire_placement(image.image_number, placement.id, paint);
        write_kitty_placement(&mut output, &image.record.image, &placement)
            .map_err(kitty_output_error)?;
        placed = true;
    }
    if !placed {
        return Err(invalid_image_data(
            "completed image is absent from its desired frame",
        ));
    }
    restore_cursor_state(&mut output, images.cursor)?;
    if let Err(error) = writer.write_all(&output).and_then(|()| writer.flush()) {
        images.needs_abort = true;
        mark_kitty_cache_uncertain(images);
        return Err(error);
    }
    Ok(())
}

/// Start the first desired image whose pixels are not in the terminal cache.
fn start_next_kitty_upload(images: &mut KittyImageCache) -> io::Result<()> {
    if images.upload.is_some() {
        return Ok(());
    }
    let Some(desired_index) = (images.next_upload_index..images.desired.len()).find(|index| {
        let paint = &images.desired[*index];
        images
            .images
            .get(&paint.content_id)
            .is_none_or(|image| !kitty_image_matches(image, &paint.record))
    }) else {
        images.next_upload_index = images.desired.len();
        return Ok(());
    };
    images.next_upload_index = desired_index + 1;
    let paint = &images.desired[desired_index];
    let content_id = paint.content_id;
    let record = Arc::clone(&paint.record);
    let image_number = take_nonzero_id(
        &mut images.next_image_number,
        "Kitty image numbers are exhausted",
    )?;
    let codec = KittyCodecUpload::new(Arc::clone(&record.image), image_number)
        .map_err(kitty_output_error)?;
    images.upload = Some(KittyUpload {
        content_id,
        record,
        codec,
    });
    Ok(())
}

/// Advance the shared Kitty upload codec at the client I/O boundary.
fn advance_kitty_upload<W: Write>(writer: &mut W, upload: &mut KittyUpload) -> io::Result<()> {
    upload.codec.advance(writer).map_err(kitty_output_error)
}

/// Return whether an upload still belongs to the newest desired frame.
fn upload_is_desired(upload: &KittyUpload, desired: &[ImagePaint]) -> bool {
    desired.iter().any(|paint| {
        paint.content_id == upload.content_id
            && Arc::ptr_eq(&paint.record.image, &upload.record.image)
    })
}

/// Return whether one cached image is the record the paint names.
fn kitty_image_matches(
    image: &KittyImage,
    record: &Arc<koshi_terminal::graphics::ImageRecord>,
) -> bool {
    Arc::ptr_eq(&image.record.image, &record.image)
}

/// Forget terminal-side identities after a partial or failed native write.
fn mark_kitty_cache_uncertain(images: &mut KittyImageCache) {
    images.images.clear();
    images.placements.clear();
    images.upload = None;
    images.next_upload_index = 0;
    images.next_image_number = 1;
    images.next_placement_id = 1;
    images.needs_reset = true;
}

/// Check each current placement and reject duplicate identities before writing.
fn validate_image_paints(paints: &[ImagePaint]) -> io::Result<()> {
    let mut placements = HashSet::with_capacity(paints.len());
    let mut contents: HashMap<u64, &Arc<koshi_terminal::graphics::ImageRecord>> =
        HashMap::with_capacity(paints.len());
    for paint in paints {
        if !placements.insert((paint.pane_id, paint.placement_id)) {
            return Err(invalid_image_data(
                "image placement identity is repeated in one frame",
            ));
        }
        if contents
            .insert(paint.content_id, &paint.record)
            .is_some_and(|record| !Arc::ptr_eq(&record.image, &paint.record.image))
        {
            return Err(invalid_image_data(
                "image content identity names different pixel records",
            ));
        }
        validate_image_paint(paint)?;
    }
    Ok(())
}

/// Check one image's pixel buffer, source rectangle, and destination rectangle.
fn validate_image_paint(paint: &ImagePaint) -> io::Result<()> {
    let image = &paint.record.image;
    let expected = usize::try_from(image.width)
        .ok()
        .and_then(|width| {
            usize::try_from(image.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4));
    if image.width == 0
        || image.height == 0
        || expected != Some(image.rgba.len())
        || paint.source.width == 0
        || paint.source.height == 0
        || paint.target.width == 0
        || paint.target.height == 0
        || paint
            .source
            .x
            .checked_add(paint.source.width)
            .is_none_or(|end| end > image.width)
        || paint
            .source
            .y
            .checked_add(paint.source.height)
            .is_none_or(|end| end > image.height)
    {
        return Err(invalid_image_data(
            "image geometry does not match RGBA pixels",
        ));
    }
    Ok(())
}

/// Report whether allocating this frame's new Kitty ids requires a cache reset.
fn image_ids_need_reset(
    images: &HashMap<u64, KittyImage>,
    placements: &HashMap<(PaneId, u64), KittyPlacement>,
    next_image_number: u32,
    next_placement_id: u32,
    paints: &[ImagePaint],
) -> bool {
    let mut new_images = HashSet::new();
    for paint in paints {
        if images
            .get(&paint.content_id)
            .is_none_or(|image| !kitty_image_matches(image, &paint.record))
        {
            new_images.insert(paint.content_id);
        }
    }
    let new_placements = paints
        .iter()
        .filter(|paint| !placements.contains_key(&(paint.pane_id, paint.placement_id)))
        .count();
    ids_exhausted(next_image_number, new_images.len())
        || ids_exhausted(next_placement_id, new_placements)
}

/// Report whether `count` nonzero u32 ids remain from `next`.
fn ids_exhausted(next: u32, count: usize) -> bool {
    let available = if next == 0 {
        0
    } else {
        u64::from(u32::MAX) - u64::from(next) + 1
    };
    u64::try_from(count).map_or(true, |count| count > available)
}

/// Take one nonzero u32 identity and advance its counter.
fn take_nonzero_id(next: &mut u32, exhausted: &'static str) -> io::Result<u32> {
    let id = *next;
    if id == 0 {
        return Err(invalid_image_data(exhausted));
    }
    *next = id.checked_add(1).unwrap_or(0);
    Ok(id)
}

/// Build protocol-only Kitty placement fields from one rendered paint.
fn kitty_wire_placement(
    image_number: u32,
    placement_id: u32,
    paint: &ImagePaint,
) -> KittyWirePlacement {
    KittyWirePlacement {
        image_number,
        placement_id,
        source_x: paint.source.x,
        source_y: paint.source.y,
        source_width: paint.source.width,
        source_height: paint.source.height,
        columns: u32::from(paint.target.width),
        rows: u32::from(paint.target.height),
        cell_offset_x: paint.cell_offset_x,
        cell_offset_y: paint.cell_offset_y,
        z_index: paint.z_index,
    }
}

/// Convert a Kitty output failure into the client's image I/O error type.
fn kitty_output_error(error: KittyOutputError) -> io::Error {
    match error {
        KittyOutputError::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error),
    }
}

fn restore_cursor_state<W: Write>(
    writer: &mut W,
    cursor: Option<ratatui::layout::Position>,
) -> io::Result<()> {
    if let Some(cursor) = cursor {
        write!(
            writer,
            "\x1b[{};{}H",
            u32::from(cursor.y) + 1,
            u32::from(cursor.x) + 1
        )?;
    } else {
        writer.write_all(b"\x1b[?25l")?;
    }
    Ok(())
}

/// Build an I/O error for a malformed image source rectangle.
fn invalid_image_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The crossterm command for one pane's cursor style.
///
/// Each [`Shaped`](CursorStyle::Shaped) shape-and-blink pair maps to the one
/// crossterm variant that re-emits the same DECSCUSR sequence:
/// `Shaped { shape: Bar, blink: true }` results in
/// [`BlinkingBar`](SetCursorStyle::BlinkingBar).
/// [`UserDefault`](CursorStyle::UserDefault) maps to
/// [`DefaultUserShape`](SetCursorStyle::DefaultUserShape), which hands the
/// cursor back to whatever the user configured in their own terminal.
pub(crate) fn set_cursor_style(style: CursorStyle) -> SetCursorStyle {
    let CursorStyle::Shaped { shape, blink } = style else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (shape, blink) {
        (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
        (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
        (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
    }
}

/// The outer emulator's window title for one frame: the session name, plus
/// `" | "` and the focused pane's resolved title when that pane is in
/// `snapshot.panes` and its title is a non-empty string.
///
/// Session `"quiet-lake"` with the focused pane titled `"htop"` results in
/// `"quiet-lake | htop"`. No focused pane, a focused pane missing from
/// `snapshot.panes`, no title, or an empty title all result in
/// `"quiet-lake"`.
pub(crate) fn window_title(snapshot: &RenderSnapshot) -> String {
    let focused_title = snapshot
        .client
        .focused_pane
        .and_then(|id| snapshot.panes.iter().find(|pane| pane.id == id))
        .and_then(|pane| pane.title.as_deref());
    match focused_title {
        Some(title) if !title.is_empty() => format!("{} | {title}", snapshot.session.name),
        _ => snapshot.session.name.clone(),
    }
}

#[cfg(test)]
mod tests;
