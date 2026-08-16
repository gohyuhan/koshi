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
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen, SetTitle};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::Client;
use koshi_core::geometry::Size;
use koshi_core::ids::ClientId;
use koshi_core::key::KeySequence;
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_observability::logging::init_tracing;
use koshi_renderer::snapshot::{CursorStyle, KeymapHints, RenderSnapshot, ViewerChrome};
use koshi_renderer::theme::Theme;
use koshi_renderer::{cursor_position, cursor_style, render_frame};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_terminal::state::CursorShape;

use koshi_input::keyboard::decode_key;
use koshi_link::error::CliError;

/// Paints a render snapshot into ratatui's frame buffer via the widget trait —
/// the only way to reach the frame's buffer, and exactly the shape
/// [`render_frame`] expects.
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
}

impl Widget for SnapshotWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_frame(
            self.snapshot,
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
/// it: raw mode off, default cursor shape, mouse capture and bracketed paste
/// off, alternate screen left. It runs on any exit — normal, error, or panic.
pub(crate) fn register_terminal_restore(cleanup: &TerminalCleanupGuard) {
    cleanup.register_cleanup(Box::new(|| {
        let _ = disable_raw_mode();
        // The cursor style koshi last copied out of a pane belongs to that pane,
        // not to the shell koshi exits back to: quitting while vim was inserting
        // would otherwise leave the user's own prompt wearing vim's blinking bar.
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        // Undo the mouse capture enabled at startup, so the terminal koshi exits
        // back to has its native selection and scroll again.
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
    ensure_koshi_dirs();
    let runtime_dir = koshi_link::ipc_client::runtime_dir()?;
    // Read before a session id exists, so the `logging` section can decide
    // whether a log file is opened at all, and at what level and format.
    let app = koshi_link::config::load_app_layer();
    // The interactive launch has no `--allow-other-users` to force, so the new
    // session's own `koshi.kdl` decides who may reach it.
    let session_id = koshi_link::router_client::request_new_session(&runtime_dir, profile, None)?;
    let _ = init_tracing(koshi_link::config::logging_params(app.as_ref(), session_id));
    crate::attach::attach_session(&runtime_dir, session_id)
}

/// Build the viewer half and apply `loaded`'s viewer-owned files, in one step.
///
/// It folds its own settings, resolves its chrome colors, and validates its own
/// keymap, so the palette a frame is painted in and the keys it answers are
/// this terminal's. `keybinding.kdl` is the one file that can be read and then
/// refused, so it is the one whose outcome is logged; app settings and the
/// theme are typed values that always apply.
///
/// The client owns no session, so the receiver it is handed has no sender: its
/// frames arrive over the connection instead.
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

/// Create koshi's on-disk home for this run: the config directory, at its
/// fixed per-platform location (resolved through `koshi-paths`). Failures are
/// logged and the session still starts: a terminal works without a config
/// directory.
fn ensure_koshi_dirs() {
    match koshi_paths::config_dir() {
        Some(config) => match koshi_paths::ensure_dir(&config) {
            Ok(()) => tracing::info!(path = %config.display(), "config directory ready"),
            Err(error) => {
                tracing::warn!(path = %config.display(), %error, "could not create config directory");
            }
        },
        None => tracing::warn!("no home directory found; skipping config directory setup"),
    }
}

/// Block on crossterm events and send decoded keys, mouse events, pastes and
/// every terminal resize down `inbox_tx`. Read failure means terminal hangup
/// and quits. The caller relays what comes out of the channel up its
/// connection to the session.
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
                Ok(Event::Resize(cols, rows)) => Some(RuntimeEvent::Resize {
                    client_id,
                    size: Size { cols, rows },
                }),
                Ok(Event::Mouse(mouse)) => Some(RuntimeEvent::MouseInput {
                    client_id,
                    mouse: decode_mouse(mouse),
                }),
                // The outer terminal pasted (the OS paste key): the text arrives
                // whole, so no character of it can fire a keybinding.
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
/// `last_title` and `last_cursor` hold what was last sent to the outer terminal,
/// so each is written only when it changes: focus moving to a pane with a
/// different DECSCUSR style re-emits that style. The style belongs to the
/// outer terminal, not to the frame. The bar vim asked its "terminal"
/// for is the bar the user sees.
///
/// The hint bar, the theme, the open key sequence, the hovered pane and the tab
/// strip's position all come from `client` — they are this viewer's own, and the
/// frame says nothing about them. The one thing the hint bar takes from the
/// frame is whether mouse-select is on, which decides the label that entry wears.
pub(crate) fn paint_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), B::Error> {
    let title = window_title(snapshot);
    if title != *last_title {
        let _ = execute!(io::stdout(), SetTitle(&title));
        *last_title = title;
    }
    let cursor = cursor_style(snapshot);
    if cursor != *last_cursor {
        if let Some(style) = cursor.map(set_cursor_style) {
            let _ = execute!(io::stdout(), style);
        }
        *last_cursor = cursor;
    }
    let hints = client.frame_hints(snapshot.client.mouse_select);
    let viewer = client.chrome(snapshot.client.active_tab);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            SnapshotWidget {
                snapshot,
                theme: client.theme(),
                hints: &hints,
                pending: client.pending_sequence(),
                viewer,
            },
            area,
        );
        if let Some(position) = cursor_position(snapshot, area) {
            frame.set_cursor_position(position);
        }
    })?;
    Ok(())
}

/// The crossterm command for one pane's cursor style. Crossterm's six shaped
/// variants are the same six styles a pane can ask for via DECSCUSR, so each
/// maps to exactly one: a blinking [`Bar`](CursorShape::Bar) is vim's
/// insert-mode cursor. A pane that asked for nothing maps to `DefaultUserShape`,
/// which hands the cursor back to whatever the user configured in their own
/// terminal.
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
/// the focused pane's resolved title when it has one.
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
