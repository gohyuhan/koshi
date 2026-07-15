//! The runnable `koshi` binary: terminal setup, genesis, and the event loop.
//!
//! Startup enters raw mode + the alternate screen + mouse capture (all restored
//! on drop or panic by a cleanup guard), builds the runtime, and seeds one
//! session/tab/shell pane. A background thread turns crossterm key and mouse
//! events into inbox events; the main loop drains the inbox, applies each event
//! to the runtime, and repaints when the render scheduler says a frame is due.
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
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::ClientId;
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_observability::logging::{init_tracing, TracingOptions};
use koshi_pty::backend::state::PtyBackend;
use koshi_pty::portable::PortablePtyBackend;
use koshi_renderer::snapshot::{CursorStyle, RenderSnapshot};
use koshi_renderer::{cursor_position, cursor_style, render_frame};
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::state::Runtime;
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
/// or the shell exits, then restore the terminal. Errors surface to `main`.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _tracing = init_tracing(TracingOptions::from_env())?;
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }));
    let _panic_guard = install_panic_hook(&cleanup);
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    // Capture mouse events so koshi can hit-test clicks (tabs, panes, scroll).
    // This is terminal-global: while on, programs inside panes and native text
    // selection do not see the mouse until koshi forwards it.
    execute!(io::stdout(), EnableMouseCapture)?;

    // Build the runtime, handing it the cleanup guard so it restores the
    // terminal when the runtime drops at the end of this function.
    let (inbox_tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let backend: Arc<dyn PtyBackend> = Arc::new(PortablePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let mut runtime = Runtime::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
        cleanup,
        // The stock default split direction; a loaded config supplies its own.
        Direction::Right,
    );

    let (cols, rows) = size()?;
    let viewport = Size { cols, rows };

    // The ratatui terminal owns the output side; the renderer paints its buffer.
    // Construct it BEFORE spawning the shell, so a size-ioctl failure here can't
    // orphan a live child — after the spawn below, no fallible step precedes the
    // kill guard.
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Genesis: one session, one tab, one shell pane sized to the terminal.
    let client_id = runtime.bootstrap_local(viewport, SystemTime::now())?;

    // Input thread: crossterm reads block here, feeding the inbox.
    spawn_input_thread(inbox_tx, client_id);

    // Run the loop, then tear down however it ended — see [`teardown`].
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_loop(&mut runtime, &mut terminal, client_id)
    }));
    teardown(&mut runtime, outcome)?;
    Ok(())
}

/// Create koshi's on-disk homes for this run: the config directory with its
/// `plugins/` tree, and the private runtime directory sockets live in
/// (owner-only on Unix). Every path resolves through `koshi-paths`, so a
/// `KOSHI_CONFIG_DIR`/`KOSHI_RUNTIME_DIR` override relocates what gets
/// created. Failures are logged and the session still starts: a terminal
/// works without a config directory.
fn ensure_koshi_dirs() {
    match koshi_paths::config_dir() {
        Some(config) => {
            for dir in [config.clone(), config.join("plugins")] {
                if let Err(error) = koshi_paths::ensure_dir(&dir) {
                    tracing::warn!(path = %dir.display(), %error, "could not create config directory");
                }
            }
        }
        None => tracing::warn!("no home directory found; skipping config directory setup"),
    }
    match koshi_paths::runtime_dir() {
        Some(runtime) => {
            if let Err(error) = koshi_paths::ensure_private_dir(&runtime) {
                tracing::warn!(path = %runtime.display(), %error, "could not create runtime directory");
            }
        }
        None => tracing::warn!("no home directory found; skipping runtime directory setup"),
    }
}

/// Tear the runtime down for whichever way the loop ended. A normal return —
/// a clean quit or the loop's own I/O error — runs staged shutdown. Explicit
/// quit uses immediate group-kill; natural/error exits use graceful group-kill;
/// both then persist and hand back the loop's result for [`run`] to
/// propagate. A caught panic takes the abrupt path — immediately group-kill
/// every child so none is orphaned, then re-raise, so the panic still unwinds
/// `runtime` and its cleanup guard restores the terminal (and the tracing
/// guard flushes logs) as before.
///
/// Generic over the loop's error type so it threads through unchanged and a
/// test can drive it with any backend.
fn teardown<E>(runtime: &mut Runtime, outcome: thread::Result<Result<(), E>>) -> Result<(), E> {
    match outcome {
        Ok(result) => {
            runtime.shutdown();
            result
        }
        Err(panic) => {
            runtime.kill_all_panes();
            resume_unwind(panic);
        }
    }
}

/// Block on crossterm events and forward decoded keys plus every terminal
/// resize into the runtime inbox. Read failure means terminal hangup and quits.
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
    runtime: &mut Runtime,
    terminal: &mut Terminal<B>,
    client_id: ClientId,
) -> Result<(), B::Error> {
    let mut last_title = String::new();
    let mut last_cursor = None;
    loop {
        let now = Instant::now();
        let next = earliest(
            runtime.next_render_wakeup(now),
            runtime.next_key_wakeup(now),
        );
        let event = match next {
            Some(timeout) => match runtime.inbox_rx().recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match runtime.inbox_rx().recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            },
        };
        let mut quit = false;
        if let Some(event) = event {
            quit |= handle_event(runtime, event).is_break();
        }
        // Apply anything else already queued before painting one frame.
        while let Ok(event) = runtime.inbox_rx().try_recv() {
            quit |= handle_event(runtime, event).is_break();
        }
        if quit || runtime.quit_requested() {
            break;
        }
        runtime.expire_key_sequences(Instant::now());
        if runtime.poll_render(Instant::now()) {
            render(
                terminal,
                runtime,
                client_id,
                &mut last_title,
                &mut last_cursor,
            )?;
        }
        if !runtime.has_active_panes() {
            break;
        }
    }
    Ok(())
}

/// Route one inbox event to its runtime handler. Returns
/// [`ControlFlow::Break`] when the event is a quit request, so the loop stops.
/// A [`RuntimeEvent::Quit`] is a terminal hangup — explicit quit travels
/// through the `core:quit` command — so it breaks the loop and leaves
/// teardown on the graceful path.
fn handle_event(runtime: &mut Runtime, event: RuntimeEvent) -> ControlFlow<()> {
    match event {
        RuntimeEvent::Quit => return ControlFlow::Break(()),
        RuntimeEvent::PtyOutput { pane_id, bytes } => runtime.handle_pty_output(pane_id, &bytes),
        RuntimeEvent::ChildExit {
            pane_id,
            status,
            exited_at,
        } => {
            let _ = runtime.handle_child_exit(pane_id, status, exited_at);
        }
        RuntimeEvent::KeyInput { client_id, chord } => {
            runtime.handle_key_input(client_id, chord, Instant::now());
        }
        RuntimeEvent::MouseInput { client_id, mouse } => {
            runtime.handle_mouse_input(client_id, mouse);
        }
        RuntimeEvent::ClientAttached {
            session_id,
            client_id,
            viewport,
            active_tab,
            attached_at,
        } => {
            let _ = runtime.handle_client_attach(
                session_id,
                client_id,
                viewport,
                active_tab,
                attached_at,
            );
        }
        RuntimeEvent::ClientDetached { client_id } => {
            let _ = runtime.handle_client_detach(client_id);
        }
        RuntimeEvent::Resize { client_id, size } => {
            let _ = runtime.handle_client_resize(client_id, size);
        }
        RuntimeEvent::Timer => runtime.expire_key_sequences(Instant::now()),
        RuntimeEvent::Ipc(envelope) | RuntimeEvent::Plugin(envelope) => {
            let _ = runtime.dispatch(envelope);
        }
    }
    ControlFlow::Continue(())
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
    runtime: &Runtime,
    client_id: ClientId,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> Result<(), B::Error> {
    let Some(snapshot) = runtime.build_snapshot(client_id) else {
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
