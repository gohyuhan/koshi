//! The outer terminal an attached client owns: the viewer built for it, the
//! thread that reads its input, and painting frames into it.
//!
//! Every item here belongs to one attached terminal. The session it is joined
//! to owns none of them.

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
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::attach::ViewerPaint;
use crate::Client;
use koshi_core::geometry::{PixelCellSize, Size};
use koshi_core::ids::ClientId;
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
    write_kitty_abort, write_kitty_delete_all, write_kitty_support_query, KittyOutputError,
    KITTY_QUERY_IMAGE_ID,
};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    build_image_cell_snapshot, build_image_paints, get_cursor_position, get_cursor_style,
    render_frame_with_image_availability, ImagePlacementKey, ImageRenderMode,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_sixel::{PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY};
use koshi_terminal::state::CursorShape;

use self::platform::{PlatformWaker, TerminalDevice};
use self::reader::InputReader;

mod image_output;
mod platform;
mod reader;

pub(crate) use self::image_output::{ImageCompatibility, ImageOutputKind, ImageOutputState};

const TERMINAL_QUERY_TIMEOUT_DURATION: Duration = Duration::from_millis(300);

/// Request a terminal's current cell dimensions in pixels.
const CELL_SIZE_QUERY_BYTES: &[u8] = b"\x1b[16t";

/// Enter the alternate screen and enable keyboard, mouse, and paste reports.
const APPLICATION_MODE_SETUP_BYTES: &[u8] = b"\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h";

/// Disable paste, mouse, keyboard, and alternate-screen modes; restore cursor state.
const APPLICATION_MODE_CLEANUP_BYTES: &[u8] =
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
    pub(crate) pending_key_sequence: Option<&'a KeySequence>,
    /// The pane this viewer's pointer is over, and where its tab strip sits.
    pub(crate) chrome: ViewerChrome,
    /// The region solve committed with the frame being painted.
    pub(crate) committed_regions: &'a CommittedRegions,
    /// The image mode selected for this outer terminal.
    pub(crate) image_mode: ImageRenderMode,
    /// The image placements whose native bytes are ready for this frame.
    pub(crate) available_image_placement_keys: Option<&'a [ImagePlacementKey]>,
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, render_area: Rect, render_buffer: &mut Buffer) {
        render_frame_with_image_availability(
            self.snapshot,
            self.committed_regions,
            self.theme,
            self.hints,
            self.pending_key_sequence,
            self.chrome,
            self.image_mode,
            self.available_image_placement_keys,
            render_area,
            render_buffer,
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
        palette_color_count: usize,
        /// Maximum Sixel width in pixels, or `None` when the host reports no limit.
        max_pixel_width: Option<u32>,
        /// Maximum Sixel height in pixels, or `None` when the host reports no limit.
        max_pixel_height: Option<u32>,
    },
}

/// A failure while painting a frame or emitting its native image data.
#[derive(Debug)]
pub(crate) enum PaintError<BackendError> {
    /// The ratatui backend did not accept the frame buffer.
    Backend(BackendError),
    /// The native image writer did not accept its output.
    Image(io::Error),
}

impl GraphicsSupport {
    /// Map the terminal capability to the renderer's image mode.
    pub(crate) fn get_image_render_mode(self) -> ImageRenderMode {
        match self {
            Self::Unsupported => ImageRenderMode::Placeholder,
            Self::Kitty => ImageRenderMode::Native,
            Self::Iterm | Self::Sixel { .. } => ImageRenderMode::Native,
        }
    }

    /// Convert the selected connection-local protocol to its attach report.
    pub(crate) const fn build_graphics_capabilities(self) -> GraphicsCapabilities {
        match self {
            Self::Unsupported => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: false,
                supports_sixel: false,
            },
            Self::Kitty => GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
            Self::Iterm => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: true,
                supports_sixel: false,
            },
            Self::Sixel { .. } => GraphicsCapabilities {
                supports_kitty: false,
                supports_iterm: false,
                supports_sixel: true,
            },
        }
    }
}

/// The values gathered by one bounded terminal capability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalProbe {
    /// The preferred protocol proved by the terminal.
    graphics_support: GraphicsSupport,
    /// The cell dimensions reported by the terminal, if valid.
    cell_size: Option<PixelCellSize>,
}

impl TerminalProbe {
    /// Build the result for a terminal that answered no capability query.
    const fn unsupported() -> Self {
        Self {
            graphics_support: GraphicsSupport::Unsupported,
            cell_size: None,
        }
    }
}

/// Select the cell dimensions that a native-image terminal may report.
fn resolve_initial_cell_size(
    graphics_support: GraphicsSupport,
    probed_cell_size: Option<PixelCellSize>,
    locally_measured_cell_size: Option<PixelCellSize>,
) -> Option<PixelCellSize> {
    if matches!(graphics_support, GraphicsSupport::Unsupported) {
        None
    } else {
        probed_cell_size.or(locally_measured_cell_size)
    }
}

/// Coordinates cell-size requests with resize reports on one terminal.
#[derive(Debug)]
pub(crate) struct CellSizeQuery {
    /// The most recent measurement accepted for the current viewport.
    current_cell_size: Option<PixelCellSize>,
    /// Whether a CSI 16t request has not received its reply.
    is_reply_pending: bool,
    /// Whether the pending reply belongs to an older viewport.
    should_discard_pending_reply: bool,
    /// Whether this attachment can write terminal queries.
    is_query_enabled: bool,
}

impl CellSizeQuery {
    /// Build a coordinator with the measurement captured before Attach and
    /// whether a cell-size query is already outstanding.
    pub(crate) fn from_current_measurement(
        current_cell_size: Option<PixelCellSize>,
        is_query_enabled: bool,
        is_reply_pending: bool,
    ) -> Self {
        Self {
            current_cell_size: is_query_enabled.then_some(current_cell_size).flatten(),
            is_reply_pending: is_query_enabled && is_reply_pending,
            should_discard_pending_reply: false,
            is_query_enabled,
        }
    }

    /// Return the measurement to place in the next Attach.
    pub(crate) fn get_current_cell_size(&self) -> Option<PixelCellSize> {
        self.current_cell_size
    }

    /// Invalidate or replace the measurement for a resized viewport.
    ///
    /// A pending reply is discarded because CSI 16t carries no request id. A
    /// fresh request is needed only when the resize has no usable local metric.
    pub(crate) fn update_cell_size_for_resize(
        &mut self,
        measured_cell_size: Option<PixelCellSize>,
    ) -> bool {
        let resized_cell_size = self
            .is_query_enabled
            .then_some(measured_cell_size)
            .flatten();
        self.current_cell_size = resized_cell_size;
        if self.is_reply_pending {
            self.should_discard_pending_reply = true;
            false
        } else {
            resized_cell_size.is_none()
        }
    }

    /// Accept a reply, returning the value to send to the session and whether
    /// a fresh query must be written after discarding an older reply.
    pub(crate) fn accept_cell_size_reply(
        &mut self,
        reported_cell_size: PixelCellSize,
    ) -> (Option<PixelCellSize>, bool) {
        if !self.is_reply_pending {
            return (None, false);
        }
        self.is_reply_pending = false;
        if self.should_discard_pending_reply {
            self.should_discard_pending_reply = false;
            return (None, self.current_cell_size.is_none());
        }
        self.current_cell_size = Some(reported_cell_size);
        (Some(reported_cell_size), false)
    }

    /// Write one CSI 16t request at the attachment loop's output boundary.
    pub(crate) fn request_cell_size(&mut self) {
        if !self.is_query_enabled || self.is_reply_pending {
            return;
        }
        let mut writer = io::stdout().lock();
        if let Err(request_error) = self.write_cell_size_request(&mut writer) {
            tracing::warn!(%request_error, "could not request terminal cell dimensions");
        }
    }

    /// Write one request through `writer`, retaining pending state after an
    /// attempted write until a matching ordered reply is consumed.
    fn write_cell_size_request<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if !self.is_query_enabled || self.is_reply_pending || self.current_cell_size.is_some() {
            return Ok(());
        }
        self.is_reply_pending = true;
        writer.write_all(CELL_SIZE_QUERY_BYTES)?;
        writer.flush()
    }
}

/// The input, mode, and capability-query owner for one attached terminal.
pub(crate) struct TerminalOwner {
    /// Native graphics support proved by the terminal's protocol answer.
    graphics_support: GraphicsSupport,
    /// Initial pixel dimensions of one terminal cell from the probe or window metrics.
    initial_cell_size: Option<PixelCellSize>,
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
    is_activated: bool,
}

impl TerminalOwner {
    /// Open the controlling terminal and probe its capabilities when image
    /// support is enabled. A native-image terminal whose probe did not receive
    /// a cell size uses the window's pixel dimensions through [`read_local_cell_size`].
    /// With piped input and output, build an unsupported owner without opening
    /// `/dev/tty`; that client reads no keys and writes its frame to the pipe.
    pub(crate) fn open_terminal_owner(supports_native_images: bool) -> Result<Self, String> {
        let input_is_terminal = io::stdin().is_terminal();
        let output_is_terminal = io::stdout().is_terminal();
        let (graphics_support, initial_cell_size, terminal, reader, waker) =
            if needs_terminal_device(input_is_terminal, output_is_terminal) {
                let (mut terminal, event_source) = TerminalDevice::open_terminal_device()
                    .map_err(|open_error| format!("could not open the terminal: {open_error}"))?;
                let mut reader = InputReader::from_event_source(event_source);
                let terminal_probe = if supports_native_images {
                    resolve_graphics_support_for_output(output_is_terminal, || {
                        run_with_raw_mode(
                            &mut terminal,
                            |terminal| terminal.enter_raw_mode(),
                            |terminal| probe_terminal(terminal, &mut reader),
                            |terminal| terminal.enter_cooked_mode(),
                        )
                    })
                    .map_err(|probe_error| {
                        format!("could not probe terminal graphics support: {probe_error}")
                    })?
                } else {
                    TerminalProbe::unsupported()
                };
                let locally_measured_cell_size = if terminal_probe.cell_size.is_some()
                    || matches!(
                        terminal_probe.graphics_support,
                        GraphicsSupport::Unsupported
                    ) {
                    None
                } else {
                    read_local_cell_size()
                };
                let initial_cell_size = resolve_initial_cell_size(
                    terminal_probe.graphics_support,
                    terminal_probe.cell_size,
                    locally_measured_cell_size,
                );
                let waker = reader.create_waker();
                (
                    terminal_probe.graphics_support,
                    initial_cell_size,
                    Some(terminal),
                    Some(reader),
                    Some(waker),
                )
            } else {
                (GraphicsSupport::Unsupported, None, None, None, None)
            };
        let image_cleanup_claimed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            graphics_support,
            initial_cell_size,
            output_is_terminal,
            terminal: Arc::new(Mutex::new(terminal)),
            reader,
            waker,
            shutdown: Arc::new(AtomicBool::new(false)),
            application_modes_active: Arc::new(AtomicBool::new(false)),
            image_cleanup_claimed,
            input_thread: None,
            is_activated: false,
        })
    }

    /// Return the native graphics support proved by the terminal probe.
    pub(crate) fn get_graphics_support(&self) -> GraphicsSupport {
        self.graphics_support
    }

    /// Return the cell dimensions captured during the initial terminal probe.
    #[allow(dead_code)]
    pub(crate) fn get_initial_cell_size(&self) -> Option<PixelCellSize> {
        self.initial_cell_size
    }

    /// Build the cell-size coordinator used after this terminal attaches.
    pub(crate) fn build_cell_size_query(&self) -> CellSizeQuery {
        CellSizeQuery::from_current_measurement(
            self.initial_cell_size,
            self.output_is_terminal
                && !matches!(self.graphics_support, GraphicsSupport::Unsupported),
            false,
        )
    }

    /// Register panic-safe image cleanup and terminal restoration.
    pub(crate) fn register_restore(&self, cleanup: &TerminalCleanupGuard) {
        let terminal = Arc::clone(&self.terminal);
        let graphics_support = self.graphics_support;
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
                graphics_support,
                &application_modes_active,
                &image_cleanup_claimed,
            );
        }));
    }

    /// Enable terminal modes and start input delivery after Attach succeeds.
    pub(crate) fn activate(
        &mut self,
        runtime_event_sender: mpsc::SyncSender<RuntimeEvent>,
        client_id: ClientId,
        should_read_input: bool,
    ) -> Result<(), String> {
        if self.is_activated {
            return Err("terminal owner was already activated".to_string());
        }
        self.is_activated = true;
        {
            let mut terminal_guard = lock_terminal(&self.terminal);
            if let Some(terminal) = terminal_guard.as_mut() {
                terminal.enter_raw_mode().map_err(|raw_mode_error| {
                    format!("could not enter terminal raw mode: {raw_mode_error}")
                })?;
                if self.output_is_terminal {
                    self.application_modes_active.store(true, Ordering::Release);
                    enable_terminal_modes(terminal, self.graphics_support).map_err(
                        |mode_enable_error| {
                            format!("could not enable terminal modes: {mode_enable_error}")
                        },
                    )?;
                }
            } else if self.output_is_terminal || should_read_input {
                return Err("terminal owner was already restored".to_string());
            }
        }
        if !should_read_input {
            return Ok(());
        }

        let mut input_reader = self
            .reader
            .take()
            .ok_or_else(|| "terminal input reader is unavailable".to_string())?;
        let shutdown = Arc::clone(&self.shutdown);
        let panic_event_sender = runtime_event_sender.clone();
        self.input_thread = Some(
            thread::Builder::new()
                .name("koshi-terminal-input".to_string())
                .spawn(move || {
                    let input_thread_result = catch_unwind(AssertUnwindSafe(|| {
                        run_terminal_input(
                            &mut input_reader,
                            &runtime_event_sender,
                            client_id,
                            &shutdown,
                        );
                    }));
                    if input_thread_result.is_err() {
                        let _ = panic_event_sender.send(RuntimeEvent::Quit);
                    }
                })
                .map_err(|thread_spawn_error| {
                    format!("could not spawn the terminal input thread: {thread_spawn_error}")
                })?,
        );
        Ok(())
    }

    /// Stop input delivery and restore the host terminal state.
    pub(crate) fn shutdown(mut self) {
        self.stop_terminal_owner();
    }

    /// Signal and join the input thread, then restore every terminal mode.
    fn stop_terminal_owner(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(waker) = &self.waker {
            let _ = waker.wake();
        }
        if let Some(input_thread_handle) = self.input_thread.take() {
            let _ = input_thread_handle.join();
        }
        restore_shared_terminal(
            &self.terminal,
            self.graphics_support,
            &self.application_modes_active,
            &self.image_cleanup_claimed,
        );
    }
}

impl Drop for TerminalOwner {
    fn drop(&mut self) {
        self.stop_terminal_owner();
    }
}

/// Whether input or output needs access to the controlling terminal.
fn needs_terminal_device(input_is_terminal: bool, output_is_terminal: bool) -> bool {
    input_is_terminal || output_is_terminal
}

/// Probe only when standard output is the terminal that will receive images.
fn resolve_graphics_support_for_output(
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
fn run_with_raw_mode<Terminal, OperationOutput>(
    terminal: &mut Terminal,
    enter_raw: impl FnOnce(&mut Terminal) -> io::Result<()>,
    operation: impl FnOnce(&mut Terminal) -> io::Result<OperationOutput>,
    enter_cooked: impl FnOnce(&mut Terminal) -> io::Result<()>,
) -> io::Result<OperationOutput> {
    enter_raw(terminal)?;
    let operation_result = operation(terminal);
    let restore_result = enter_cooked(terminal);
    match (operation_result, restore_result) {
        (Err(operation_error), Err(restore_error)) => {
            tracing::warn!(%restore_error, "could not restore terminal after failed operation");
            Err(operation_error)
        }
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Ok(operation_output), Ok(())) => Ok(operation_output),
    }
}

/// Lock the shared terminal and recover its value after a poisoned lock.
fn lock_terminal(
    terminal_mutex: &Mutex<Option<TerminalDevice>>,
) -> std::sync::MutexGuard<'_, Option<TerminalDevice>> {
    terminal_mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restore the shared terminal once, waiting for an in-progress terminal write.
fn restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let terminal_device = lock_terminal(shared_terminal).take();
    let Some(mut terminal) = terminal_device else {
        return;
    };
    restore_terminal(
        &mut terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore without blocking a panic hook on the thread that holds the terminal.
fn try_restore_shared_terminal(
    shared_terminal: &Mutex<Option<TerminalDevice>>,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut terminal_guard = match shared_terminal.try_lock() {
        Ok(terminal_guard) => terminal_guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            write_fallback_terminal_cleanup(
                graphics_support,
                application_modes_active,
                image_cleanup_claimed,
            );
            return;
        }
    };
    let Some(mut terminal) = terminal_guard.take() else {
        return;
    };
    drop(terminal_guard);
    restore_terminal(
        &mut terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Restore application modes and the platform's cooked terminal mode.
fn restore_terminal(
    terminal: &mut TerminalDevice,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    if let Err(application_restore_error) = restore_application_modes(
        terminal,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    ) {
        tracing::warn!(%application_restore_error, "could not restore terminal application modes");
    }
    if let Err(cooked_mode_error) = terminal.enter_cooked_mode() {
        tracing::warn!(%cooked_mode_error, "could not restore terminal cooked mode");
    }
}

/// Claim and restore this attachment's application-level terminal modes once.
fn restore_application_modes<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    if !application_modes_active.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    write_terminal_cleanup(writer, graphics_support, image_cleanup_claimed)
}

/// Claim the one Kitty cleanup allowed for an attachment.
fn claim_image_cleanup(image_cleanup_claimed: &AtomicBool) -> bool {
    !image_cleanup_claimed.swap(true, Ordering::AcqRel)
}

/// Gather every capability reply available before one shared deadline.
fn probe_terminal<EventSourceType: reader::EventSource, OutputWriter: Write>(
    writer: &mut OutputWriter,
    reader: &mut InputReader<EventSourceType>,
) -> io::Result<TerminalProbe> {
    write_terminal_probe_queries(writer)?;
    let probe_deadline = Instant::now() + TERMINAL_QUERY_TIMEOUT_DURATION;
    let mut probe_replies = ProbeReplies::default();
    loop {
        let remaining_probe_timeout = probe_deadline.saturating_duration_since(Instant::now());
        if !reader.wait_for_event(Some(remaining_probe_timeout), is_probe_event)? {
            break;
        }
        let probe_event = reader.read_matching_event(is_probe_event)?;
        probe_replies.observe(probe_event);
        if Instant::now() >= probe_deadline {
            break;
        }
    }
    Ok(probe_replies.build_terminal_probe())
}

/// Replies collected while capability queries share one deadline.
#[derive(Default)]
struct ProbeReplies {
    supports_kitty: bool,
    supports_iterm_file_output: bool,
    supports_iterm_sixel: bool,
    supports_da1_sixel: bool,
    sixel_palette_color_count: Option<Result<u32, koshi_input::host::GraphicAttributeError>>,
    sixel_geometry: Option<Result<(u32, u32), koshi_input::host::GraphicAttributeError>>,
    cell_size: Option<PixelCellSize>,
}

impl ProbeReplies {
    /// Retain one parsed event that belongs to the capability probe.
    fn observe(&mut self, probe_event: Event) {
        match probe_event {
            Event::KittyGraphicsReply(kitty_reply)
                if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID =>
            {
                self.supports_kitty |= kitty_reply.is_successful;
            }
            Event::TerminalFeatures(terminal_features) => {
                self.supports_iterm_file_output |=
                    iterm_feature_string_supports_file(&terminal_features);
                self.supports_iterm_sixel |=
                    iterm_feature_string_supports_sixel(&terminal_features);
            }
            Event::PrimaryDeviceAttributes(device_attributes) => {
                self.supports_da1_sixel |= device_attributes
                    .get(1..)
                    .is_some_and(|attribute_numbers| attribute_numbers.contains(&4));
            }
            Event::SixelGraphicsAttributeReply(sixel_attribute_reply) => {
                match sixel_attribute_reply {
                    koshi_input::host::GraphicAttributeReply::Palette(palette_color_count) => {
                        if self.sixel_palette_color_count.is_none() {
                            self.sixel_palette_color_count = Some(palette_color_count);
                        }
                    }
                    koshi_input::host::GraphicAttributeReply::Geometry(pixel_dimensions) => {
                        if self.sixel_geometry.is_none() {
                            self.sixel_geometry = Some(pixel_dimensions);
                        }
                    }
                }
            }
            Event::CellSize(cell_size) => {
                if self.cell_size.is_none() {
                    self.cell_size = Some(cell_size);
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
    fn build_terminal_probe(self) -> TerminalProbe {
        let graphics_support = if self.supports_kitty {
            GraphicsSupport::Kitty
        } else if self.supports_iterm_file_output {
            GraphicsSupport::Iterm
        } else {
            self.build_sixel_graphics_support()
                .unwrap_or(GraphicsSupport::Unsupported)
        };
        TerminalProbe {
            graphics_support,
            cell_size: self.cell_size,
        }
    }

    /// Build bounded Sixel support from the terminal's positive evidence.
    fn build_sixel_graphics_support(&self) -> Option<GraphicsSupport> {
        let is_sixel_advertised = self.supports_da1_sixel
            || self.supports_iterm_sixel
            || matches!(self.sixel_geometry, Some(Ok(_)));
        if !is_sixel_advertised {
            return None;
        }
        let palette_color_count = match self.sixel_palette_color_count {
            Some(Ok(palette_color_count)) if palette_color_count < 2 => return None,
            Some(Ok(palette_color_count)) => palette_color_count.min(256) as usize,
            Some(Err(_)) | None => 2,
        };
        let (max_pixel_width, max_pixel_height) = match self.sixel_geometry {
            Some(Ok((pixel_width, pixel_height))) => (
                (pixel_width != 0).then_some(pixel_width),
                (pixel_height != 0).then_some(pixel_height),
            ),
            Some(Err(_)) | None => (None, None),
        };
        Some(GraphicsSupport::Sixel {
            palette_color_count,
            max_pixel_width,
            max_pixel_height,
        })
    }
}

/// Write every bounded capability query and flush them as one probe batch.
fn write_terminal_probe_queries<W: Write>(writer: &mut W) -> io::Result<()> {
    write_kitty_support_query(writer).map_err(convert_kitty_output_error)?;
    writer.write_all(ITERM_CAPABILITIES_QUERY)?;
    writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    writer.write_all(SIXEL_PALETTE_QUERY)?;
    writer.write_all(SIXEL_GEOMETRY_QUERY)?;
    writer.write_all(CELL_SIZE_QUERY_BYTES)?;
    writer.flush()
}

/// Return whether an event belongs to the capability probe.
fn is_probe_event(probe_event: &Event) -> bool {
    matches!(probe_event, Event::KittyGraphicsReply(kitty_reply) if kitty_reply.image_id == KITTY_QUERY_IMAGE_ID)
        || matches!(probe_event, Event::TerminalFeatures(_))
        || matches!(probe_event, Event::PrimaryDeviceAttributes(_))
        || matches!(probe_event, Event::SixelGraphicsAttributeReply(_))
        || matches!(probe_event, Event::CellSize(_))
}

/// Request the terminal modes used by the attached viewer.
fn enable_terminal_modes<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
) -> io::Result<()> {
    if matches!(graphics_support, GraphicsSupport::Sixel { .. }) {
        writer.write_all(image_output::get_sixel_mode_save_bytes())?;
    }
    writer.write_all(APPLICATION_MODE_SETUP_BYTES)?;
    writer.flush()
}

/// Write every application-level terminal reset in reverse setup order.
fn write_terminal_cleanup<W: Write>(
    writer: &mut W,
    graphics_support: GraphicsSupport,
    image_cleanup_claimed: &AtomicBool,
) -> io::Result<()> {
    let mut first_io_error = None;
    if matches!(graphics_support, GraphicsSupport::Kitty)
        && claim_image_cleanup(image_cleanup_claimed)
    {
        retain_first_io_error(
            &mut first_io_error,
            write_kitty_abort(writer).map_err(convert_kitty_output_error),
        );
        retain_first_io_error(
            &mut first_io_error,
            write_kitty_delete_all(writer).map_err(convert_kitty_output_error),
        );
    }
    if matches!(graphics_support, GraphicsSupport::Sixel { .. }) {
        retain_first_io_error(&mut first_io_error, image_output::write_image_abort(writer));
        retain_first_io_error(
            &mut first_io_error,
            writer.write_all(image_output::get_sixel_mode_restore_bytes()),
        );
    }
    retain_first_io_error(
        &mut first_io_error,
        writer.write_all(APPLICATION_MODE_CLEANUP_BYTES),
    );
    retain_first_io_error(&mut first_io_error, writer.flush());
    match first_io_error {
        Some(io_error) => Err(io_error),
        None => Ok(()),
    }
}

/// Keep the first terminal I/O failure while cleanup attempts every reset.
fn retain_first_io_error(first_io_error: &mut Option<io::Error>, io_result: io::Result<()>) {
    if let Err(io_error) = io_result {
        if first_io_error.is_none() {
            *first_io_error = Some(io_error);
        }
    }
}

/// Write application resets when the panic hook cannot take the terminal lock.
fn write_fallback_terminal_cleanup(
    graphics_support: GraphicsSupport,
    application_modes_active: &AtomicBool,
    image_cleanup_claimed: &AtomicBool,
) {
    let mut stdout = io::stdout();
    let _ = restore_application_modes(
        &mut stdout,
        graphics_support,
        application_modes_active,
        image_cleanup_claimed,
    );
}

/// Read semantic terminal events until shutdown or input failure.
fn run_terminal_input(
    input_reader: &mut InputReader,
    runtime_event_sender: &mpsc::SyncSender<RuntimeEvent>,
    client_id: ClientId,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Acquire) {
        let runtime_event = match input_reader.read_matching_event(|_| true) {
            Ok(host_event) => build_terminal_runtime_event(client_id, host_event),
            Err(input_read_error)
                if input_read_error.kind() == io::ErrorKind::Interrupted
                    && shutdown.load(Ordering::Acquire) =>
            {
                break;
            }
            Err(input_read_error) if input_read_error.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(input_read_error) => {
                tracing::warn!(%input_read_error, "could not read terminal input");
                Some(RuntimeEvent::Quit)
            }
        };
        if let Some(runtime_event) = runtime_event {
            let is_quit_event = matches!(runtime_event, RuntimeEvent::Quit);
            if runtime_event_sender.send(runtime_event).is_err() || is_quit_event {
                break;
            }
        }
    }
}

/// Convert one host-terminal event into the runtime event the viewer consumes.
fn build_terminal_runtime_event(client_id: ClientId, host_event: Event) -> Option<RuntimeEvent> {
    match host_event {
        Event::Key(key_event) => decode_key(key_event).map(|key_chord| RuntimeEvent::KeyInput {
            client_id,
            chord: key_chord,
        }),
        Event::CellSize(cell_size) => Some(RuntimeEvent::CellSize {
            client_id,
            cell_size,
        }),
        Event::WindowResized(window_size) => {
            Some(build_resize_runtime_event(client_id, window_size))
        }
        Event::Mouse(mouse_event) => Some(RuntimeEvent::MouseInput {
            client_id,
            mouse_input: decode_mouse(mouse_event),
        }),
        Event::Paste(pasted_text) => Some(RuntimeEvent::HostPaste {
            client_id,
            pasted_text,
        }),
        Event::FocusIn
        | Event::FocusOut
        | Event::PrimaryDeviceAttributes(_)
        | Event::TerminalFeatures(_)
        | Event::SixelGraphicsAttributeReply(_)
        | Event::KittyGraphicsReply(_) => None,
    }
}

/// Build the runtime resize event for one host size report.
fn build_resize_runtime_event(client_id: ClientId, window_size: WindowSize) -> RuntimeEvent {
    let viewport_size = Size {
        column_count: window_size.column_count,
        row_count: window_size.row_count,
    };
    RuntimeEvent::Resize {
        client_id,
        viewport_size,
        pane_area: Some(crate::compute_core_pane_area(viewport_size)),
        cell_size: compute_pixel_cell_size(window_size),
    }
}

/// Read one cell's pixel dimensions from the output terminal's window size.
///
/// `None` when the window size cannot be read, when the platform reports no
/// pixel dimensions, or when they do not divide evenly across the grid.
pub(crate) fn read_local_cell_size() -> Option<PixelCellSize> {
    platform::read_window_size()
        .ok()
        .and_then(compute_pixel_cell_size)
}

/// Derive one cell's pixel dimensions from a host window report only when the
/// complete pixel dimensions divide evenly across the reported grid.
fn compute_pixel_cell_size(window_size: WindowSize) -> Option<PixelCellSize> {
    let pixel_width = window_size.pixel_width?;
    let pixel_height = window_size.pixel_height?;
    if window_size.column_count == 0
        || window_size.row_count == 0
        || pixel_width % window_size.column_count != 0
        || pixel_height % window_size.row_count != 0
    {
        return None;
    }
    PixelCellSize::from_pixel_dimensions(
        pixel_width / window_size.column_count,
        pixel_height / window_size.row_count,
    )
}

/// Build the viewer half and apply `loaded_config`'s viewer-owned files, in one step.
///
/// `client_id` is the id this viewer's input events and commands carry.
/// `viewport_size` is this terminal's size in cells. `frame_delivery_receiver` is the frame feed; a
/// client owns no session, so the receiver it is handed has no sender and its
/// frames arrive over the connection instead. `terminal_cleanup_guard` is the guard that
/// restores the outer terminal.
///
/// `loaded_config.app_config_layer` and `loaded_config.theme_config_layer` fold into the viewer's settings and chrome
/// colors and always apply. `loaded_config.keybindings` is validated: a verdict other
/// than [`Apply`](koshi_config::conflict::KeymapVerdict::Apply) logs a warning
/// naming `koshi keys conflicts`, an `Apply` logs `"keybinding.kdl applied"`,
/// and a `None` keymap layer logs nothing.
pub(crate) fn build_client_with_loaded_config(
    client_id: ClientId,
    viewport_size: Size,
    frame_delivery_receiver: mpsc::Receiver<koshi_renderer::snapshot::Delivery>,
    terminal_cleanup_guard: TerminalCleanupGuard,
    loaded_config: koshi_link::config::LoadedConfig,
) -> Client {
    let mut viewer_client = Client::from_client_id_and_viewport(
        client_id,
        viewport_size,
        frame_delivery_receiver,
        terminal_cleanup_guard,
    );
    match viewer_client.load_startup_config(
        loaded_config.app_config_layer,
        loaded_config.theme_config_layer,
        loaded_config.keybindings,
    ) {
        Some(report) if report.get_verdict() != koshi_config::conflict::KeymapVerdict::Apply => {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }
    viewer_client
}

/// Draw `snapshot` into `terminal`, keeping the outer terminal's window title
/// and cursor style in step with the focused pane.
///
/// The theme comes from `client`, and so does the hint bar, built for
/// `frame_paint.lock_mode` and `frame_paint.is_mouse_selection_enabled`. The hovered pane, the
/// tab strip's position and the open key sequence come from `frame_paint`.
/// `committed_regions` is the geometry shared by the painter and cursor
/// placement for this frame.
///
/// `last_window_title` and `last_cursor_style` store the title and cursor style used to
/// decide whether the next frame needs a control write. Both are read before
/// the buffer paint and updated after it succeeds: a changed title writes
/// `SetTitle`, and a changed cursor style writes `SetCursorStyle`. A frame
/// that [`get_cursor_style`] names no style stores `None` and writes no style
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
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let mut image_output_state = ImageOutputState::disabled();
    paint_frame_with_images(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        &mut image_output_state,
        None,
        last_window_title,
        last_cursor_style,
    )
    .map(|_| ())
}

/// Paint one frame and schedule native images when the outer terminal supports
/// them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frame_with_images<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
) -> Result<bool, PaintError<B::Error>> {
    let mut stdout = io::stdout();
    paint_frame_with_writer(
        &mut stdout,
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        image_mode,
        image_output_state,
        cell_size,
        last_window_title,
        last_cursor_style,
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
    image_output_state: &mut ImageOutputState,
    cell_size: Option<PixelCellSize>,
    last_window_title: &mut String,
    last_cursor_style: &mut Option<CursorStyle>,
) -> Result<bool, PaintError<B::Error>> {
    let window_title_text = build_window_title(snapshot);
    let is_window_title_changed = window_title_text != *last_window_title;
    let cursor_style = get_cursor_style(snapshot);
    let is_cursor_style_changed = cursor_style != *last_cursor_style;
    let keymap_hints = client.build_frame_hints(
        frame_paint.lock_mode,
        frame_paint.is_mouse_selection_enabled,
    );
    let terminal_size = terminal.size().map_err(PaintError::Backend)?;
    let mut render_area = Rect::new(0, 0, terminal_size.width, terminal_size.height);
    let mut hardware_cursor_position =
        get_cursor_position(snapshot, committed_regions, render_area);
    let is_native_image_output = image_output_state.output_kind().is_some();
    image_output_state.set_host_terminal_size(terminal_size.width, terminal_size.height);
    let image_paint_commands = build_image_paints(snapshot, committed_regions, render_area);
    let image_cell_composition_snapshot = image_output_state
        .output_kind()
        .filter(|output_kind| {
            output_kind.uses_cell_composition() && !image_paint_commands.is_empty()
        })
        .and_then(|_| build_image_cell_snapshot(snapshot, committed_regions, render_area))
        .map(Arc::new);
    if is_native_image_output
        && !image_output_state.prepare_frame(
            &image_paint_commands,
            image_cell_composition_snapshot,
            cell_size,
        )
    {
        return Ok(false);
    }
    let is_native_image_commit =
        is_native_image_output && image_output_state.native_commit_pending();
    let native_image_output_bytes = if is_native_image_commit {
        match image_output_state.frame_output(hardware_cursor_position) {
            Ok(native_image_output_bytes) => native_image_output_bytes,
            Err(image_frame_error) => {
                image_output_state.fail_frame_commit();
                return Err(PaintError::Image(image_frame_error));
            }
        }
    } else {
        Vec::new()
    };
    if is_native_image_commit {
        if let Err(synchronized_update_error) = execute!(writer, BeginSynchronizedUpdate) {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(PaintError::Image(synchronized_update_error));
        }
    }
    let paint_result = (|| {
        if is_native_image_output {
            if tracing::enabled!(tracing::Level::DEBUG) {
                for image_placement_key in image_output_state
                    .list_prepared_placement_keys()
                    .iter()
                    .copied()
                {
                    if let Some(image_compatibility) =
                        image_output_state.get_image_compatibility(image_placement_key)
                    {
                        if image_compatibility != ImageCompatibility::default() {
                            tracing::debug!(
                                ?image_placement_key,
                                ?image_compatibility,
                                "native image output has host compatibility limits"
                            );
                        }
                    }
                }
            }
            if image_output_state
                .write_frame_reset(writer)
                .map_err(PaintError::Image)?
            {
                // The host screen is blank after `ESC[2J`. Emptying both ratatui
                // buffers makes the next draw write every cell again.
                terminal.swap_buffers();
            }
        }
        let available_image_placement_keys =
            is_native_image_output.then(|| image_output_state.list_prepared_placement_keys());
        terminal
            .draw(|render_frame| {
                let frame_area = render_frame.area();
                render_area = frame_area;
                hardware_cursor_position =
                    get_cursor_position(snapshot, committed_regions, frame_area);
                render_frame.render_widget(
                    SnapshotWidget {
                        snapshot,
                        theme: client.get_theme(),
                        hints: &keymap_hints,
                        pending_key_sequence: frame_paint.pending_key_sequence.as_ref(),
                        chrome: frame_paint.chrome,
                        committed_regions,
                        image_mode,
                        available_image_placement_keys,
                    },
                    frame_area,
                );
                if let Some(cursor_position) = hardware_cursor_position {
                    render_frame.set_cursor_position(cursor_position);
                }
            })
            .map_err(PaintError::Backend)?;
        if is_window_title_changed {
            execute!(writer, SetTitle(&window_title_text)).map_err(PaintError::Image)?;
        }
        if is_cursor_style_changed {
            if let Some(cursor_command) = cursor_style.map(set_cursor_style) {
                execute!(writer, cursor_command).map_err(PaintError::Image)?;
            }
        }
        if is_native_image_commit {
            writer
                .write_all(&native_image_output_bytes)
                .map_err(PaintError::Image)?;
        }
        Ok(())
    })();
    if is_native_image_commit {
        if let Err(frame_paint_error) = paint_result {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(frame_paint_error);
        }
        if let Err(synchronized_flush_error) =
            execute!(writer, EndSynchronizedUpdate).and_then(|()| writer.flush())
        {
            image_output_state.fail_frame_commit();
            recover_synchronized_frame(writer);
            return Err(PaintError::Image(synchronized_flush_error));
        }
        image_output_state.commit_frame();
    } else {
        paint_result?;
    }
    if is_window_title_changed {
        *last_window_title = window_title_text;
    }
    if is_cursor_style_changed {
        *last_cursor_style = cursor_style;
    }
    Ok(true)
}

/// Cancel an incomplete terminal control string and close synchronized output.
fn recover_synchronized_frame<W: Write>(writer: &mut W) {
    let _ = image_output::write_image_abort(writer);
    let _ = execute!(writer, EndSynchronizedUpdate);
    let _ = writer.flush();
}

fn restore_cursor_state<W: Write>(
    writer: &mut W,
    cursor_position: Option<ratatui::layout::Position>,
) -> io::Result<()> {
    if let Some(cursor_position) = cursor_position {
        write!(
            writer,
            "\x1b[{};{}H",
            u32::from(cursor_position.y) + 1,
            u32::from(cursor_position.x) + 1
        )?;
    } else {
        writer.write_all(b"\x1b[?25l")?;
    }
    Ok(())
}

fn convert_kitty_output_error(kitty_error: KittyOutputError) -> io::Error {
    match kitty_error {
        KittyOutputError::Io(io_error) => io_error,
        kitty_error => io::Error::new(io::ErrorKind::InvalidData, kitty_error),
    }
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
pub(crate) fn set_cursor_style(cursor_style: CursorStyle) -> SetCursorStyle {
    let CursorStyle::Shaped { shape, blink } = cursor_style else {
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
/// `render_snapshot.pane_snapshots` and its title is a non-empty string.
///
/// Session `"quiet-lake"` with the focused pane titled `"htop"` results in
/// `"quiet-lake | htop"`. No focused pane, a focused pane missing from
/// `render_snapshot.pane_snapshots`, no title, or an empty title all result in
/// `"quiet-lake"`.
pub(crate) fn build_window_title(render_snapshot: &RenderSnapshot) -> String {
    let focused_pane_title = render_snapshot
        .client_snapshot
        .focused_pane_id
        .and_then(|focused_pane_id| {
            render_snapshot
                .pane_snapshots
                .iter()
                .find(|pane_snapshot| pane_snapshot.pane_id == focused_pane_id)
        })
        .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref());
    match focused_pane_title {
        Some(pane_title) if !pane_title.is_empty() => {
            format!(
                "{} | {pane_title}",
                render_snapshot.session_snapshot.session_name
            )
        }
        _ => render_snapshot.session_snapshot.session_name.clone(),
    }
}

#[cfg(test)]
mod tests;
