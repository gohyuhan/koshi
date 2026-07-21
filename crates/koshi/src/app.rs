//! The runnable `koshi` binary: terminal setup, genesis, and the event loop.
//!
//! Startup reads the config, installs the log subscriber the config asks for,
//! then enters raw mode + the alternate screen + mouse capture (all restored
//! on drop or panic by a cleanup guard), builds the server, and seeds one
//! session/tab/shell pane. A background thread turns crossterm key and mouse
//! events into inbox events; the main loop drains the inbox, applies each event
//! to the server, and repaints when the render scheduler says a frame is due.
//! Ctrl-Q, or the shell exiting, ends the loop.

use std::io;
use std::ops::ControlFlow;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Instant, SystemTime};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, SessionId};
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_observability::logging::{init_tracing, LoggingParams};
use koshi_pty::backend::state::PtyBackend;
use koshi_pty::portable::PortablePtyBackend;
use koshi_renderer::snapshot::{CursorStyle, RenderSnapshot};
use koshi_renderer::{cursor_position, cursor_style, render_frame};
use koshi_runtime::client::Client;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_terminal::state::CursorShape;

use crate::keys::decode_key;

/// Paints a render snapshot into ratatui's frame buffer via the widget trait —
/// the only way to reach the frame's buffer, and exactly the shape
/// [`render_frame`] expects.
struct SnapshotWidget<'a>(&'a RenderSnapshot);

impl Widget for SnapshotWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_frame(self.0, area, buf);
    }
}

/// Launch the interactive session: set up the terminal, run the loop until quit
/// or the shell exits, then restore the terminal. When `profile` names one, the
/// session opens that profile's tabs and panes; otherwise it opens one shell.
/// Errors surface to `main`.
pub fn run(profile: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    // Read the config before tracing starts, so the `logging` section can
    // decide whether a log file is opened at all, and at what level and format.
    // `load` collects its own warnings instead of logging, since there is no
    // subscriber yet; they are replayed below once one is installed.
    let (loaded, config_warnings) = crate::config::load();
    // Mint the session id up front: it names the per-session log file and is
    // the same id genesis registers below, so the filename matches the session.
    let session_id = SessionId::new();
    let logging = loaded
        .app
        .as_ref()
        .map(|app| app.logging_config())
        .unwrap_or_default();
    init_tracing(LoggingParams {
        enabled: logging.enabled,
        level: logging.level,
        format: logging.format,
        session_id,
    })?;
    // The first line written, so a log file that exists at all already says
    // which level and format the session ran under.
    tracing::info!(
        session_id = %session_id,
        level = ?logging.level,
        format = ?logging.format,
        "logging started"
    );
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }
    // Which config files were read, and how many pieces of them were skipped —
    // the warnings above say what each skip was.
    tracing::info!(
        koshi_kdl = loaded.app.is_some(),
        theme = loaded.theme.is_some(),
        keybinding_kdl = loaded.keybindings.is_some(),
        skipped = config_warnings.len(),
        "config files read"
    );
    ensure_koshi_dirs();

    // Restore the terminal on any exit — normal, error, or panic.
    let cleanup = TerminalCleanupGuard::new();
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
    let _panic_guard = install_panic_hook(&cleanup);
    // Each step below is one koshi has no way to work around: without it there
    // is no surface to draw on, so the failure is logged as an error naming the
    // step and the launch ends here.
    enable_raw_mode().inspect_err(|error| tracing::error!(%error, "could not enter raw mode"))?;
    execute!(io::stdout(), EnterAlternateScreen)
        .inspect_err(|error| tracing::error!(%error, "could not enter the alternate screen"))?;
    // Capture mouse events so koshi can hit-test clicks (tabs, panes, scroll).
    // This is terminal-global: while on, programs inside panes and native text
    // selection do not see the mouse until koshi forwards it.
    execute!(io::stdout(), EnableMouseCapture)
        .inspect_err(|error| tracing::error!(%error, "could not capture the mouse"))?;
    // Ask the outer terminal to bracket its pastes, so the OS paste key
    // arrives as one block of text instead of a burst of keystrokes.
    execute!(io::stdout(), EnableBracketedPaste)
        .inspect_err(|error| tracing::error!(%error, "could not enable bracketed paste"))?;
    tracing::info!("terminal ready");

    // Build the server. The cleanup guard stays out of it — the outer
    // terminal is the client's, so the client built below holds the guard.
    let (inbox_tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let backend: Arc<dyn PtyBackend> = Arc::new(PortablePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let mut server = Server::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
        // The stock default; the loaded config below supplies the real one.
        Direction::Right,
    );

    // Apply the config (loaded before tracing above) before genesis, so the
    // first session already sees the configured split direction, theme, and
    // keymap. A rejected keybinding file leaves the built-in keymap in place.
    // `keybinding.kdl` is the one file that can be read and then refused, so it
    // is the one with an outcome to report. App settings and the theme are typed
    // values that always apply, so the line above already accounts for them.
    match server.load_startup_config(loaded.app, loaded.theme, loaded.keybindings) {
        Some(report) if report.verdict() != koshi_config::conflict::KeymapVerdict::Apply => {
            tracing::warn!("keybinding.kdl was not applied; run `koshi keys conflicts` to see why");
        }
        Some(_) => tracing::info!("keybinding.kdl applied"),
        None => {}
    }

    let (cols, rows) =
        size().inspect_err(|error| tracing::error!(%error, "could not read the terminal size"))?;
    let viewport = Size { cols, rows };

    // The ratatui terminal owns the output side; the renderer paints its buffer.
    // Construct it BEFORE spawning the shell, so a size-ioctl failure here can't
    // orphan a live child — after the spawn below, no fallible step precedes the
    // kill guard.
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .inspect_err(|error| tracing::error!(%error, "could not build the output terminal"))?;

    // Genesis: a named profile's tabs and panes, or one shell sized to the
    // terminal. A profile that cannot be loaded or launched falls back to the
    // single shell, so the terminal always comes up.
    let now = SystemTime::now();
    // A profile that will not launch falls back to the single shell, so it is a
    // warning; the single shell failing to start has nothing left to fall back
    // to, so it is an error and the launch ends.
    let client_id = match profile.and_then(crate::config::load_profile) {
        Some(template) => match server.bootstrap_profile(session_id, template, viewport, now) {
            Ok(client_id) => Ok(client_id),
            Err(err) => {
                tracing::warn!(%err, "profile could not launch; starting a single shell");
                server.bootstrap_local(session_id, viewport, now)
            }
        },
        None => server.bootstrap_local(session_id, viewport, now),
    }
    .inspect_err(|error| tracing::error!(%error, "could not start the session"))?;
    tracing::info!(session_id = %session_id, client_id = %client_id, "session started");

    // The client half: the view side of the process. It subscribes to the
    // server's events and holds the cleanup guard, since the outer terminal
    // it restores is the client's.
    let events_rx = server.subscribe(EventFilter::All);
    let mut client = Client::new(client_id, viewport, events_rx, cleanup);

    // Input thread: crossterm reads block here, feeding the inbox.
    spawn_input_thread(inbox_tx, client_id);

    // Run the loop, then tear down however it ended — see [`teardown`].
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_loop(&mut server, &mut client, &mut terminal)
    }));
    teardown(&mut server, outcome)
        .inspect_err(|error| tracing::error!(%error, "the render loop failed"))?;
    Ok(())
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

/// Tear the server down for whichever way the loop ended. A normal return —
/// a clean quit or the loop's own I/O error — runs staged shutdown. Explicit
/// quit uses immediate group-kill; natural/error exits use graceful group-kill;
/// both then persist and hand back the loop's result for [`run`] to
/// propagate. A caught panic takes the abrupt path — immediately group-kill
/// every child so none is orphaned, then re-raise, so the panic still unwinds
/// `server` and its cleanup guard restores the terminal (and the tracing
/// guard flushes logs) as before.
///
/// Generic over the loop's error type so it threads through unchanged and a
/// test can drive it with any backend.
fn teardown<E>(server: &mut Server, outcome: thread::Result<Result<(), E>>) -> Result<(), E> {
    match outcome {
        Ok(result) => {
            tracing::info!("shutting down");
            server.shutdown();
            result
        }
        Err(panic) => {
            // Nothing anticipated this, so there is no fallback to take: every
            // child is killed and the panic is re-raised.
            tracing::error!("koshi panicked; killing every pane");
            server.kill_all_panes();
            resume_unwind(panic);
        }
    }
}

/// Block on crossterm events and forward decoded keys plus every terminal
/// resize into the server inbox. Read failure means terminal hangup and quits.
fn spawn_input_thread(inbox_tx: mpsc::Sender<RuntimeEvent>, client_id: ClientId) {
    thread::spawn(move || loop {
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
    });
}

/// The event loop: block until an event is due (bounded by the next render
/// deadline), apply it and any others already queued, repaint if due, and stop
/// once a `core:quit` binding fires, a [`RuntimeEvent::Quit`] (terminal
/// hangup) arrives, or no pane remains. Generic over the backend so a test
/// can drive it headlessly.
fn run_loop<B: Backend>(
    server: &mut Server,
    client: &mut Client,
    terminal: &mut Terminal<B>,
) -> Result<(), B::Error> {
    let mut last_title = String::new();
    let mut last_cursor = None;
    loop {
        let now = Instant::now();
        let next = earliest(
            earliest(server.next_render_wakeup(now), server.next_key_wakeup(now)),
            server.next_selection_scroll_wakeup(now),
        );
        let event = match next {
            Some(timeout) => match server.inbox_rx().recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match server.inbox_rx().recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            },
        };
        let mut quit = false;
        if let Some(event) = event {
            quit |= apply_event(server, client, event).is_break();
        }
        // Apply anything else already queued before painting one frame.
        while let Ok(event) = server.inbox_rx().try_recv() {
            quit |= apply_event(server, client, event).is_break();
        }
        // The embedded client renders from server snapshots, so the events its
        // subscription delivers are discarded here; the discard keeps its
        // bounded queue from filling.
        client.discard_events();
        // Escapes aimed at this client's outer terminal — including an OSC 52
        // clipboard write — reach stdout before a queued quit is honored.
        // They draw nothing and do not change renderer state.
        if let Some(bytes) = server.take_host_writes(client.id()) {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&bytes);
            let _ = stdout.flush();
        }
        if quit || server.quit_requested() {
            break;
        }
        server.expire_key_sequences(Instant::now());
        server.expire_selection_scrolls(Instant::now());
        if server.poll_render(Instant::now()) {
            render(
                terminal,
                server,
                client.id(),
                &mut last_title,
                &mut last_cursor,
            )?;
        }
        if !server.has_active_panes() {
            break;
        }
    }
    Ok(())
}

/// Hand one inbox event to the server, first letting the client record its
/// own terminal's new size when the event is that client's resize. Returns
/// [`ControlFlow::Break`] when the event is a quit request, so the loop stops.
fn apply_event(server: &mut Server, client: &mut Client, event: RuntimeEvent) -> ControlFlow<()> {
    if let RuntimeEvent::Resize { client_id, size } = &event {
        if *client_id == client.id() {
            client.set_viewport(*size);
        }
    }
    server.handle_runtime_event(event)
}

fn earliest(
    left: Option<std::time::Duration>,
    right: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
        (None, None) => None,
    }
}

/// Paint one frame for `client_id`'s viewport, placing the hardware cursor,
/// matching its style to the focused pane's, and keeping the outer terminal
/// emulator's window title on `<session> | <focused pane title>`. Generic over
/// the backend so a test can render into an in-memory buffer; the title and
/// cursor-style escapes go to the real stdout and are skipped when unchanged,
/// so frames that move nothing emit nothing.
fn render<B: Backend>(
    terminal: &mut Terminal<B>,
    server: &Server,
    client_id: ClientId,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), B::Error> {
    let Some(snapshot) = server.build_snapshot(client_id) else {
        return Ok(());
    };
    let title = window_title(&snapshot);
    if title != *last_title {
        let _ = execute!(io::stdout(), SetTitle(&title));
        *last_title = title;
    }
    // The pane owns the look of the cursor sitting in it, so koshi passes the
    // focused pane's DECSCUSR style straight out to the terminal it is itself
    // running in: the bar vim asked its "terminal" for is the bar the user sees.
    // Focus moving to a pane with a different style re-emits it, since the style
    // is a property of the outer terminal, not of the frame.
    let cursor = cursor_style(&snapshot);
    if cursor != *last_cursor {
        if let Some(style) = cursor.map(set_cursor_style) {
            let _ = execute!(io::stdout(), style);
        }
        *last_cursor = cursor;
    }
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(SnapshotWidget(&snapshot), area);
        if let Some(position) = cursor_position(&snapshot, area) {
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
fn set_cursor_style(style: CursorStyle) -> SetCursorStyle {
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
fn window_title(snapshot: &RenderSnapshot) -> String {
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
