//! The outer terminal an attached client owns: the viewer built for it, the
//! thread that reads its input, and painting frames into it.
//!
//! Every item here belongs to one attached terminal. The session it is joined
//! to owns none of them.

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen, SetTitle};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::attach::ViewerPaint;
use crate::{core_pane_area, Client};
use koshi_core::geometry::Size;
use koshi_core::ids::ClientId;
use koshi_core::key::KeySequence;
use koshi_input::keyboard::decode_key;
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    cursor_position, cursor_style, image_paints, render_frame_with_images, ImagePaint,
    ImageRenderMode,
};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_terminal::state::CursorShape;

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

/// The graphics capability selected from the outer terminal environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphicsSupport {
    /// Paint image coverage with the fixed unsupported-image text.
    Unsupported,
    /// Emit Kitty raw-RGBA image commands after the text buffer is painted.
    Kitty,
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

/// Detect the one native image protocol this client emits.
pub(crate) fn detect_graphics_support() -> GraphicsSupport {
    let term = std::env::var("TERM").ok();
    let kitty_window_id = std::env::var_os("KITTY_WINDOW_ID").is_some();
    graphics_support_from_environment(term.as_deref(), kitty_window_id)
}

/// Resolve graphics support from values supplied by a caller or test.
pub(crate) fn graphics_support_from_environment(
    term: Option<&str>,
    kitty_window_id: bool,
) -> GraphicsSupport {
    if term == Some("xterm-kitty") || kitty_window_id {
        GraphicsSupport::Kitty
    } else {
        GraphicsSupport::Unsupported
    }
}

/// Register the hook that puts the outer terminal back the way koshi found
/// it: raw mode off, cursor shown and back to its default shape, mouse capture
/// and bracketed paste off, alternate screen left. It runs on any exit —
/// normal, error, or panic.
pub(crate) fn register_terminal_restore(cleanup: &TerminalCleanupGuard) {
    cleanup.register_cleanup(Box::new(|| {
        let _ = disable_raw_mode();
        // Writes `ESC[?25h`. A frame that placed no cursor hid it, and leaving
        // the alternate screen does not bring it back.
        let _ = execute!(io::stdout(), Show);
        // Drops the cursor style koshi last copied out of a pane and puts the
        // cursor back to the shape the user's own terminal is set to.
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        // Releases the mouse capture enabled at startup: the terminal koshi
        // exits back to has its own selection and scroll again.
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }));
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

/// Spawn the `koshi-input` thread: it blocks on crossterm events and sends
/// each one down `inbox_tx`, tagged with `client_id`. The caller relays what
/// comes out of the channel up its connection.
///
/// A key becomes [`RuntimeEvent::KeyInput`]; a key [`decode_key`] returns
/// `None` for is dropped. A terminal resize becomes [`RuntimeEvent::Resize`]
/// carrying the pane area the built-in two-row UI leaves for the new size. A
/// mouse event becomes [`RuntimeEvent::MouseInput`]. A bracketed paste arrives
/// whole as [`RuntimeEvent::HostPaste`]. Every other event is dropped. A read
/// error sends [`RuntimeEvent::Quit`].
///
/// The thread ends after it sends `Quit`, and when `inbox_tx`'s receiver is
/// gone.
///
/// # Panics
///
/// Panics when the thread cannot be spawned.
pub(crate) fn spawn_input_thread(inbox_tx: mpsc::Sender<RuntimeEvent>, client_id: ClientId) {
    let _ = thread::Builder::new()
        .name("koshi-input".to_string())
        .spawn(move || loop {
            let runtime_event = match event::read() {
                Ok(Event::Key(key)) => {
                    let Some(chord) = decode_key(key) else {
                        continue;
                    };
                    Some(RuntimeEvent::KeyInput { client_id, chord })
                }
                Ok(Event::Resize(cols, rows)) => {
                    let size = Size { cols, rows };
                    Some(RuntimeEvent::Resize {
                        client_id,
                        size,
                        pane_area: Some(core_pane_area(size)),
                    })
                }
                Ok(Event::Mouse(mouse)) => Some(RuntimeEvent::MouseInput {
                    client_id,
                    mouse: decode_mouse(mouse),
                }),
                Ok(Event::Paste(text)) => Some(RuntimeEvent::HostPaste { client_id, text }),
                Ok(_) => None,
                Err(_) => Some(RuntimeEvent::Quit),
            };
            if let Some(runtime_event) = runtime_event {
                let quit = matches!(runtime_event, RuntimeEvent::Quit);
                if inbox_tx.send(runtime_event).is_err() || quit {
                    break;
                }
            }
        })
        .expect("spawn terminal input thread");
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
    paint_frame_with_images(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        ImageRenderMode::Placeholder,
        last_title,
        last_cursor,
    )
}

/// Paint one frame and emit native Kitty images when the outer terminal
/// supports them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frame_with_images<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    image_mode: ImageRenderMode,
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
        write_kitty_frame(writer, &paints, hardware_cursor).map_err(PaintError::Image)?;
    }
    Ok(())
}

const KITTY_IMAGE_CHUNK_BYTES: usize = 3_072;

/// Write one complete Kitty image frame, remove the previous frame's images,
/// and restore or hide the outer terminal cursor.
fn write_kitty_frame<W: Write>(
    writer: &mut W,
    paints: &[ImagePaint],
    cursor: Option<ratatui::layout::Position>,
) -> io::Result<()> {
    let frame_result = write_kitty_frame_body(writer, paints);
    let cursor_result = restore_cursor_state(writer, cursor);
    let flush_result = writer.flush();
    match frame_result {
        Err(error) => Err(error),
        Ok(()) => cursor_result.and(flush_result),
    }
}

fn write_kitty_frame_body<W: Write>(writer: &mut W, paints: &[ImagePaint]) -> io::Result<()> {
    writer.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
    for paint in paints {
        write!(
            writer,
            "\x1b[{};{}H",
            u32::from(paint.target.y) + 1,
            u32::from(paint.target.x) + 1
        )?;
        write_kitty_image(writer, paint)?;
    }
    Ok(())
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

/// Stream one clipped RGBA image as Kitty raw data in bounded chunks.
fn write_kitty_image<W: Write>(writer: &mut W, paint: &ImagePaint) -> io::Result<()> {
    let source = paint.source;
    let image = &paint.record.image;
    let image_width = usize::try_from(image.width)
        .map_err(|_| invalid_image_data("image width cannot fit this platform"))?;
    let image_height = usize::try_from(image.height)
        .map_err(|_| invalid_image_data("image height cannot fit this platform"))?;
    let source_x = usize::try_from(source.x)
        .map_err(|_| invalid_image_data("source x cannot fit this platform"))?;
    let source_y = usize::try_from(source.y)
        .map_err(|_| invalid_image_data("source y cannot fit this platform"))?;
    let source_width = usize::try_from(source.width)
        .map_err(|_| invalid_image_data("source width cannot fit this platform"))?;
    let source_height = usize::try_from(source.height)
        .map_err(|_| invalid_image_data("source height cannot fit this platform"))?;
    let source_x_end = source_x
        .checked_add(source_width)
        .ok_or_else(|| invalid_image_data("source x range overflows"))?;
    let source_y_end = source_y
        .checked_add(source_height)
        .ok_or_else(|| invalid_image_data("source y range overflows"))?;
    if source_width == 0
        || source_height == 0
        || source_x_end > image_width
        || source_y_end > image_height
    {
        return Err(invalid_image_data(
            "source rectangle is outside image pixels",
        ));
    }
    let row_stride = image_width
        .checked_mul(4)
        .ok_or_else(|| invalid_image_data("image row stride overflows"))?;
    let source_row_bytes = source_width
        .checked_mul(4)
        .ok_or_else(|| invalid_image_data("source row size overflows"))?;
    let source_byte_x = source_x
        .checked_mul(4)
        .ok_or_else(|| invalid_image_data("source x byte offset overflows"))?;
    let source_bytes = source_row_bytes
        .checked_mul(source_height)
        .ok_or_else(|| invalid_image_data("source byte count overflows"))?;
    let mut chunk = Vec::with_capacity(KITTY_IMAGE_CHUNK_BYTES);
    let mut first = true;
    let mut remaining_source_bytes = source_bytes;
    for row in source_y..source_y_end {
        let row_start = row
            .checked_mul(row_stride)
            .and_then(|value| value.checked_add(source_byte_x))
            .ok_or_else(|| invalid_image_data("source row offset overflows"))?;
        let row_end = row_start
            .checked_add(source_row_bytes)
            .ok_or_else(|| invalid_image_data("source row end overflows"))?;
        let row_bytes = image
            .rgba
            .get(row_start..row_end)
            .ok_or_else(|| invalid_image_data("source rectangle is outside RGBA pixels"))?;
        let mut remaining = row_bytes;
        while !remaining.is_empty() {
            let room = KITTY_IMAGE_CHUNK_BYTES - chunk.len();
            let take = room.min(remaining.len());
            chunk.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            remaining_source_bytes = remaining_source_bytes
                .checked_sub(take)
                .ok_or_else(|| invalid_image_data("source byte count underflows"))?;
            if chunk.len() == KITTY_IMAGE_CHUNK_BYTES {
                let final_chunk = remaining_source_bytes == 0;
                write_kitty_chunk(writer, paint, &chunk, first, final_chunk)?;
                first = false;
                chunk.clear();
                if final_chunk {
                    return Ok(());
                }
            }
        }
    }
    write_kitty_chunk(writer, paint, &chunk, first, true)
}

/// Write one Kitty base64 chunk and its APC terminator.
fn write_kitty_chunk<W: Write>(
    writer: &mut W,
    paint: &ImagePaint,
    bytes: &[u8],
    first: bool,
    final_chunk: bool,
) -> io::Result<()> {
    let more = if final_chunk { 0 } else { 1 };
    if first {
        write!(
            writer,
            "\x1b_Ga=T,f=32,s={},v={},{}{}c={},r={},C=1,z={},q=2,m={};",
            paint.source.width,
            paint.source.height,
            paint
                .cell_offset_x
                .map_or(String::new(), |offset| format!("X={offset},")),
            paint
                .cell_offset_y
                .map_or(String::new(), |offset| format!("Y={offset},")),
            paint.target.width,
            paint.target.height,
            paint.z_index,
            more
        )?;
    } else {
        write!(writer, "\x1b_Gm={more};")?;
    }
    writer.write_all(STANDARD.encode(bytes).as_bytes())?;
    writer.write_all(b"\x1b\\")
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
