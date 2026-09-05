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
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Compress, Compression, FlushCompress, Status};
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
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::KeySequence;
use koshi_input::host::{Event, WindowSize};
use koshi_input::keyboard::decode_key;
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    cursor_position, cursor_style, image_paints, render_frame_with_images, ImagePaint,
    ImageRenderMode, ImageSourceRect,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_terminal::state::CursorShape;

use self::platform::{PlatformWaker, TerminalDevice};
use self::reader::InputReader;

mod platform;
mod reader;

const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(300);

/// Image id reserved for the startup Kitty graphics query.
const KITTY_QUERY_IMAGE_ID: u32 = u32::MAX;

/// A one-pixel RGB Kitty query that stores no terminal-side image.
const KITTY_SUPPORT_QUERY: &[u8] = b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

/// Delete all Kitty image data owned by the active terminal screen.
const KITTY_DELETE_ALL: &[u8] = b"\x1b_Ga=d,d=A,q=2;\x1b\\";

/// Request primary device attributes after the Kitty graphics query.
const PRIMARY_DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

/// Enter the alternate screen and enable keyboard, mouse, and paste reports.
const APPLICATION_MODE_SETUP: &[u8] = b"\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h";

/// Disable paste, mouse, keyboard, and alternate-screen modes; restore cursor state.
const APPLICATION_MODE_CLEANUP: &[u8] =
    b"\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q";

/// Paints a render snapshot into ratatui's frame buffer. Every field is handed
/// straight to [`render_frame_with_images`].
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
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_frame_with_images(
            self.snapshot,
            self.committed_regions,
            self.theme,
            self.hints,
            self.pending,
            self.viewer,
            self.image_mode,
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

/// One zlib-compressed Kitty upload advanced between attachment-loop passes.
struct KittyUpload {
    /// Connection-local content identity received from the session.
    content_id: u64,
    /// Record retained while its compressed bytes are generated and sent.
    record: Arc<koshi_terminal::graphics::ImageRecord>,
    /// Kitty image number assigned to this transmission.
    image_number: u32,
    /// Incremental zlib encoder for the RGBA bytes.
    compressor: Compress,
    /// Number of raw RGBA bytes consumed by `compressor`.
    input_offset: usize,
    /// Compressed bytes waiting to be sent.
    compressed: Vec<u8>,
    /// First unsent byte in `compressed`.
    compressed_offset: usize,
    /// Whether the zlib stream reached its end marker.
    compression_complete: bool,
    /// Whether at least one Kitty data chunk reached the writer.
    transmission_started: bool,
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
        }
    }
}

/// The input, mode, and capability-query owner for one attached terminal.
pub(crate) struct TerminalOwner {
    /// Native graphics support proved by the terminal's protocol answer.
    graphics: GraphicsSupport,
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
    /// Ensures one path deletes this attachment's Kitty images before restore.
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
        let (graphics, terminal, reader, waker) =
            if terminal_device_needed(input_is_terminal, output_is_terminal) {
                let (mut terminal, source) = TerminalDevice::open()
                    .map_err(|error| format!("could not open the terminal: {error}"))?;
                let mut reader = InputReader::new(source);
                let graphics = graphics_support_for_output(output_is_terminal, || {
                    with_raw_mode(
                        &mut terminal,
                        |terminal| terminal.enter_raw_mode(),
                        |terminal| probe_kitty_graphics(terminal, &mut reader),
                        |terminal| terminal.enter_cooked_mode(),
                    )
                })
                .map_err(|error| format!("could not probe terminal graphics support: {error}"))?;
                let waker = reader.waker();
                (graphics, Some(terminal), Some(reader), Some(waker))
            } else {
                (GraphicsSupport::Unsupported, None, None, None)
            };
        let image_cleanup_claimed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            graphics,
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
                    enable_terminal_modes(terminal)
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
    probe: impl FnOnce() -> io::Result<GraphicsSupport>,
) -> io::Result<GraphicsSupport> {
    if output_is_terminal {
        probe()
    } else {
        Ok(GraphicsSupport::Unsupported)
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

/// Claim the one image cleanup allowed for an attachment.
fn claim_image_cleanup(claimed: &AtomicBool) -> bool {
    !claimed.swap(true, Ordering::AcqRel)
}

/// Send a bounded Kitty query and use DA1 as the unsupported-terminal fence.
fn probe_kitty_graphics(
    terminal: &mut TerminalDevice,
    reader: &mut InputReader,
) -> io::Result<GraphicsSupport> {
    write_kitty_graphics_query(terminal)?;
    if !reader.poll(Some(TERMINAL_QUERY_TIMEOUT), is_kitty_probe_event)? {
        return Ok(GraphicsSupport::Unsupported);
    }
    match reader.read(is_kitty_probe_event)? {
        Event::KittyGraphicsReply(reply) => Ok(if reply.ok {
            GraphicsSupport::Kitty
        } else {
            GraphicsSupport::Unsupported
        }),
        Event::PrimaryDeviceAttributes => Ok(GraphicsSupport::Unsupported),
        _ => unreachable!("the probe filter accepts only Kitty and DA1 replies"),
    }
}

/// Write the Kitty query followed by the DA1 fence and flush both together.
fn write_kitty_graphics_query<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(KITTY_SUPPORT_QUERY)?;
    writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    writer.flush()
}

/// Return whether an event completes the Kitty graphics support probe.
fn is_kitty_probe_event(event: &Event) -> bool {
    matches!(event, Event::KittyGraphicsReply(reply) if reply.image_id == KITTY_QUERY_IMAGE_ID)
        || *event == Event::PrimaryDeviceAttributes
}

/// Request the terminal modes used by the attached viewer.
fn enable_terminal_modes<W: Write>(writer: &mut W) -> io::Result<()> {
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
    if graphics == GraphicsSupport::Kitty && claim_image_cleanup(image_cleanup_claimed) {
        retain_first_io_error(&mut first_error, writer.write_all(KITTY_DELETE_ALL));
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
        Event::WindowResized(resize) => Some(resize_runtime_event(client_id, resize)),
        Event::Mouse(mouse) => Some(RuntimeEvent::MouseInput {
            client_id,
            mouse: decode_mouse(mouse),
        }),
        Event::Paste(text) => Some(RuntimeEvent::HostPaste { client_id, text }),
        Event::FocusIn
        | Event::FocusOut
        | Event::PrimaryDeviceAttributes
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
    }
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
    paint_frame_with_images(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        &mut images,
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
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), PaintError<B::Error>> {
    let title = window_title(snapshot);
    let title_changed = title != *last_title;
    let cursor = cursor_style(snapshot);
    let cursor_changed = cursor != *last_cursor;
    let hints = client.frame_hints_for(frame_paint.mode, frame_paint.mouse_select);
    let mut paint_area = Rect::default();
    let mut hardware_cursor = None;
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
                },
                area,
            );
            if let Some(position) = hardware_cursor {
                frame.set_cursor_position(position);
            }
        })
        .map_err(PaintError::Backend)?;
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
    if image_mode == ImageRenderMode::Native {
        let paints = image_paints(snapshot, committed_regions, paint_area);
        write_kitty_frame(writer, images, &paints, hardware_cursor).map_err(PaintError::Image)?;
    }
    Ok(())
}

const KITTY_IMAGE_CHUNK_BYTES: usize = 3_072;
const KITTY_IMAGE_CHUNKS_PER_STEP: usize = 16;
const KITTY_COMPRESSION_INPUT_BYTES_PER_STEP: usize = 262_144;
const KITTY_COMPRESSION_OUTPUT_BYTES: usize = 65_536;

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
        .map(|upload| upload.transmission_started);
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

/// Compress or transmit one bounded slice of the current Kitty upload.
pub(crate) fn advance_kitty_image<W: Write>(
    writer: &mut W,
    images: &mut KittyImageCache,
) -> io::Result<()> {
    let Some(upload) = images.upload.as_ref() else {
        return Ok(());
    };
    if !upload.transmission_started && !upload_is_desired(upload, &images.desired) {
        images.upload = None;
        reconcile_kitty_frame(writer, images)?;
        return start_next_kitty_upload(images);
    }

    let result = advance_kitty_upload(writer, images.upload.as_mut().expect("upload exists"));
    if let Err(error) = result {
        mark_kitty_cache_uncertain(images);
        return Err(error);
    }
    let complete = images.upload.as_ref().is_some_and(kitty_upload_complete);
    if !complete {
        return Ok(());
    }

    let upload = images.upload.take().expect("complete upload exists");
    images.images.insert(
        upload.content_id,
        KittyImage {
            record: upload.record,
            image_number: upload.image_number,
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
        output.extend_from_slice(KITTY_DELETE_ALL);
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
        write_kitty_image_delete(&mut output, image_number)?;
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
            write_kitty_placement_delete(&mut output, image.image_number, placement.id)?;
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
        write_kitty_placement(&mut output, image.image_number, placement_id, paint)?;
        cursor_moved = true;
    }

    let abort_upload = !output.is_empty()
        && images
            .upload
            .as_ref()
            .is_some_and(|upload| upload.transmission_started);
    if abort_upload && !reset {
        let mut framed = Vec::with_capacity(output.len() + 40);
        write_kitty_image_delete(
            &mut framed,
            images
                .upload
                .as_ref()
                .expect("a started upload is present")
                .image_number,
        )?;
        framed.extend_from_slice(&output);
        output = framed;
    }
    if !output.is_empty() {
        if cursor_moved {
            restore_cursor_state(&mut output, images.cursor)?;
        }
        if let Err(error) = writer.write_all(&output).and_then(|()| writer.flush()) {
            mark_kitty_cache_uncertain(images);
            return Err(error);
        }
    }

    images.images = staged_images;
    images.placements = staged_placements;
    images.next_image_number = next_image_number;
    images.next_placement_id = next_placement_id;
    images.needs_reset = false;
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
        write_kitty_placement(&mut output, image.image_number, placement.id, paint)?;
        placed = true;
    }
    if !placed {
        return Err(invalid_image_data(
            "completed image is absent from its desired frame",
        ));
    }
    restore_cursor_state(&mut output, images.cursor)?;
    if let Err(error) = writer.write_all(&output).and_then(|()| writer.flush()) {
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
    images.upload = Some(KittyUpload {
        content_id,
        record,
        image_number,
        compressor: Compress::new(Compression::fast(), true),
        input_offset: 0,
        compressed: Vec::with_capacity(KITTY_COMPRESSION_OUTPUT_BYTES),
        compressed_offset: 0,
        compression_complete: false,
        transmission_started: false,
    });
    Ok(())
}

/// Return whether an upload still belongs to the newest desired frame.
fn upload_is_desired(upload: &KittyUpload, desired: &[ImagePaint]) -> bool {
    desired.iter().any(|paint| {
        paint.content_id == upload.content_id && Arc::ptr_eq(&paint.record, &upload.record)
    })
}

/// Return whether one cached image is the record the paint names.
fn kitty_image_matches(
    image: &KittyImage,
    record: &Arc<koshi_terminal::graphics::ImageRecord>,
) -> bool {
    Arc::ptr_eq(&image.record, record)
}

/// Advance compression and write no more than one Kitty output budget.
fn advance_kitty_upload<W: Write>(writer: &mut W, upload: &mut KittyUpload) -> io::Result<()> {
    if !upload.compression_complete
        && upload.compressed.len() - upload.compressed_offset <= KITTY_IMAGE_CHUNK_BYTES
    {
        compress_kitty_upload(upload)?;
    }
    write_kitty_upload_chunks(writer, upload)
}

/// Compress at most 256 KiB of RGBA while retaining a bounded send queue.
fn compress_kitty_upload(upload: &mut KittyUpload) -> io::Result<()> {
    if upload.compressed_offset != 0 {
        upload.compressed.copy_within(upload.compressed_offset.., 0);
        upload
            .compressed
            .truncate(upload.compressed.len() - upload.compressed_offset);
        upload.compressed_offset = 0;
    }
    let rgba = &upload.record.image.rgba;
    let input_end = upload
        .input_offset
        .saturating_add(KITTY_COMPRESSION_INPUT_BYTES_PER_STEP)
        .min(rgba.len());
    let flush = if input_end == rgba.len() {
        FlushCompress::Finish
    } else {
        FlushCompress::None
    };
    let mut output = [0u8; KITTY_COMPRESSION_OUTPUT_BYTES];
    loop {
        let input_before = upload.compressor.total_in();
        let output_before = upload.compressor.total_out();
        let status = upload
            .compressor
            .compress(&rgba[upload.input_offset..input_end], &mut output, flush)
            .map_err(|error| {
                invalid_image_data_owned(format!("could not compress image: {error}"))
            })?;
        let consumed = usize::try_from(upload.compressor.total_in() - input_before)
            .map_err(|_| invalid_image_data("compressed input count does not fit this process"))?;
        let produced = usize::try_from(upload.compressor.total_out() - output_before)
            .map_err(|_| invalid_image_data("compressed output count does not fit this process"))?;
        upload.input_offset = upload
            .input_offset
            .checked_add(consumed)
            .ok_or_else(|| invalid_image_data("compressed input count overflowed"))?;
        upload
            .compressed
            .try_reserve(produced)
            .map_err(|_| invalid_image_data("compressed image bytes cannot be allocated"))?;
        upload.compressed.extend_from_slice(&output[..produced]);
        if status == Status::StreamEnd {
            upload.compression_complete = true;
            return Ok(());
        }
        if upload.input_offset == input_end && input_end < rgba.len() {
            return Ok(());
        }
        if consumed == 0 && produced == 0 {
            return Err(invalid_image_data("image compression made no progress"));
        }
    }
}

/// Write up to sixteen 3,072-byte compressed Kitty chunks.
fn write_kitty_upload_chunks<W: Write>(writer: &mut W, upload: &mut KittyUpload) -> io::Result<()> {
    let mut output = Vec::with_capacity(KITTY_IMAGE_CHUNKS_PER_STEP * 4_160);
    let mut next_offset = upload.compressed_offset;
    let mut first = !upload.transmission_started;
    let mut chunks = 0usize;
    while chunks < KITTY_IMAGE_CHUNKS_PER_STEP {
        let available = upload.compressed.len() - next_offset;
        if available == 0 || (!upload.compression_complete && available <= KITTY_IMAGE_CHUNK_BYTES)
        {
            break;
        }
        let chunk_len = available.min(KITTY_IMAGE_CHUNK_BYTES);
        let end = next_offset + chunk_len;
        let more = u8::from(!upload.compression_complete || end < upload.compressed.len());
        write_kitty_upload_chunk(
            &mut output,
            upload,
            first,
            more,
            &upload.compressed[next_offset..end],
        )?;
        first = false;
        next_offset = end;
        chunks += 1;
    }
    if output.is_empty() {
        return Ok(());
    }
    writer.write_all(&output)?;
    writer.flush()?;
    upload.compressed_offset = next_offset;
    upload.transmission_started = true;
    Ok(())
}

/// Write one first or continuation chunk from a zlib stream.
fn write_kitty_upload_chunk<W: Write>(
    writer: &mut W,
    upload: &KittyUpload,
    first: bool,
    more: u8,
    bytes: &[u8],
) -> io::Result<()> {
    if first {
        write!(
            writer,
            "\x1b_Ga=t,f=32,s={},v={},I={},q=2,o=z,m={more};",
            upload.record.image.width, upload.record.image.height, upload.image_number
        )?;
    } else {
        write!(writer, "\x1b_Gq=2,m={more};")?;
    }
    let mut encoded = [0u8; 4_096];
    let encoded_len = STANDARD
        .encode_slice(bytes, &mut encoded)
        .map_err(|_| invalid_image_data("base64 image chunk exceeded its output buffer"))?;
    writer.write_all(&encoded[..encoded_len])?;
    writer.write_all(b"\x1b\\")
}

/// Return whether compression and transmission both reached their exact ends.
fn kitty_upload_complete(upload: &KittyUpload) -> bool {
    upload.compression_complete
        && upload.compressed_offset == upload.compressed.len()
        && upload.transmission_started
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
            .is_some_and(|record| !Arc::ptr_eq(record, &paint.record))
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

/// Delete one image number and free its outer-terminal pixel data.
fn write_kitty_image_delete<W: Write>(writer: &mut W, image_number: u32) -> io::Result<()> {
    write!(writer, "\x1b_Ga=d,d=N,I={image_number},q=2;\x1b\\")
}

/// Delete one outer-terminal placement while retaining its shared pixel data.
fn write_kitty_placement_delete<W: Write>(
    writer: &mut W,
    image_number: u32,
    placement_id: u32,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b_Ga=d,d=n,I={image_number},p={placement_id},q=2;\x1b\\"
    )
}

/// Place one uploaded image with this frame's clip and destination geometry.
fn write_kitty_placement<W: Write>(
    writer: &mut W,
    image_number: u32,
    placement_id: u32,
    paint: &ImagePaint,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b_Ga=p,I={},p={},x={},y={},w={},h={},",
        image_number,
        placement_id,
        paint.source.x,
        paint.source.y,
        paint.source.width,
        paint.source.height,
    )?;
    if let Some(offset) = paint.cell_offset_x {
        write!(writer, "X={offset},")?;
    }
    if let Some(offset) = paint.cell_offset_y {
        write!(writer, "Y={offset},")?;
    }
    write!(
        writer,
        "c={},r={},C=1,z={},q=2;\x1b\\",
        paint.target.width, paint.target.height, paint.z_index,
    )
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

/// Build an invalid-image error that includes a dependency failure.
fn invalid_image_data_owned(message: String) -> io::Error {
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
