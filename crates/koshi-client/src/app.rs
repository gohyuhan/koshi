//! The bare `koshi` launch: ask the router for a new session and attach this
//! terminal to it.
//!
//! It also holds the pieces every attached client uses — the viewer, the
//! terminal input thread, and painting a frame.

use std::io;
use std::sync::mpsc;
use std::thread;

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
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_observability::logging::init_tracing;
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{cursor_position, cursor_style, render_frame};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_terminal::state::CursorShape;

use koshi_input::keyboard::decode_key;
use koshi_link::error::CliError;

/// Paints a render snapshot into ratatui's frame buffer. Every field is handed
/// straight to [`render_frame`].
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
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_frame(
            self.snapshot,
            self.committed_regions,
            self.theme,
            self.hints,
            self.pending,
            self.viewer,
            area,
            buf,
        );
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

/// Bare `koshi`: start or reuse the router, have it create a new session
/// server in this terminal's directory, and attach this terminal to it.
///
/// `profile` is handed to the router. A profile that will not launch falls
/// back to one shell inside the session server.
pub fn run(profile: Option<&str>) -> Result<(), CliError> {
    let runtime_dir = koshi_link::ipc_client::runtime_dir()?;
    // `koshi.kdl`'s `logging` section sets whether a log file is opened at
    // all, and at what level and format.
    let app = koshi_link::config::load_app_layer();
    // A `None` forced switch leaves who may reach the new session to that
    // session's own `koshi.kdl`; the interactive launch has no
    // `--allow-other-users` flag.
    let session_id = koshi_link::router_client::request_new_session(&runtime_dir, profile, None)?;
    let _ = init_tracing(koshi_link::config::logging_params(app.as_ref(), session_id));
    // Runs after the subscriber is installed, so its lines reach the log.
    // Creating the directory adds no `koshi.kdl`, so the layer read above sees
    // the same files either way.
    ensure_koshi_dirs();
    crate::attach::attach_session(&runtime_dir, session_id)
}

/// Build the viewer half and apply `loaded`'s viewer-owned files, in one step.
///
/// `client_id` is the id this viewer's input events and commands carry.
/// `viewport` is this terminal's size in cells. `events` is the frame feed; a
/// client owns no session, so the receiver it is handed has no sender and its
/// frames arrive over the connection instead. `cleanup` is the guard that
/// restores the outer terminal. `pane_area_supported` is stored on the client
/// as whether the attached session echoed the pane-area field.
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
    pane_area_supported: bool,
    loaded: koshi_link::config::LoadedConfig,
) -> Client {
    let mut client = Client::new(client_id, viewport, events, cleanup);
    client.set_pane_area_supported(pane_area_supported);
    match client.load_startup_config(loaded.app, loaded.theme, loaded.keybindings) {
        Some(report) if report.verdict() != koshi_config::conflict::KeymapVerdict::Apply => {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }
    client
}

/// Create the config directory at the fixed per-platform path
/// `koshi_paths::config_dir` gives.
///
/// The caller installs the tracing subscriber before this runs, so every line
/// below reaches the log. No home directory, and a create that fails, each
/// warn; a directory that is ready logs at info.
fn ensure_koshi_dirs() {
    let Some(config) = koshi_paths::config_dir() else {
        tracing::warn!("no home directory found; skipping config directory setup");
        return;
    };
    match koshi_paths::ensure_dir(&config) {
        Ok(()) => tracing::info!(path = %config.display(), "config directory ready"),
        Err(error) => {
            tracing::warn!(path = %config.display(), %error, "could not create config directory");
        }
    }
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
/// `last_title` and `last_cursor` record the title and cursor style committed
/// with the last successful frame paint; the style belongs to the outer
/// terminal, not to the frame. Both are read before the buffer paint and
/// written after it succeeds: a changed title goes out as `SetTitle`, a
/// changed cursor style as `SetCursorStyle`. A frame that [`cursor_style`]
/// names no style for records `None` and sends nothing.
///
/// # Errors
///
/// Returns the backend's error when the buffer paint fails, leaving
/// `last_title` and `last_cursor` as they were. A failed title or cursor-style
/// write is ignored.
pub(crate) fn paint_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), B::Error> {
    let title = window_title(snapshot);
    let title_changed = title != *last_title;
    let cursor = cursor_style(snapshot);
    let cursor_changed = cursor != *last_cursor;
    let hints = client.frame_hints_for(frame_paint.mode, frame_paint.mouse_select);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            SnapshotWidget {
                snapshot,
                theme: client.theme(),
                hints: &hints,
                pending: frame_paint.pending.as_ref(),
                viewer: frame_paint.chrome,
                committed_regions,
            },
            area,
        );
        if let Some(position) = cursor_position(snapshot, committed_regions, area) {
            frame.set_cursor_position(position);
        }
    })?;
    if title_changed {
        let _ = execute!(io::stdout(), SetTitle(&title));
        *last_title = title;
    }
    if cursor_changed {
        if let Some(style) = cursor.map(set_cursor_style) {
            let _ = execute!(io::stdout(), style);
        }
        *last_cursor = cursor;
    }
    Ok(())
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
