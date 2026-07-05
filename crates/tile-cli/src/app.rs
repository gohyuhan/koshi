//! The runnable `tile` binary: terminal setup, genesis, and the event loop.
//!
//! Startup enters raw mode + the alternate screen (restored on drop or panic by
//! a cleanup guard), builds the runtime, and seeds one session/tab/shell pane.
//! A background thread turns crossterm key events into inbox events; the main
//! loop drains the inbox, applies each event to the runtime, and repaints when
//! the render scheduler says a frame is due. Ctrl-Q, or the shell exiting, ends
//! the loop.

use std::io;
use std::ops::ControlFlow;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Instant, SystemTime};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use tile_core::geometry::Size;
use tile_core::ids::ClientId;
use tile_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use tile_observability::logging::{init_tracing, TracingOptions};
use tile_pty::backend::state::PtyBackend;
use tile_pty::portable::PortablePtyBackend;
use tile_renderer::snapshot::RenderSnapshot;
use tile_renderer::{cursor_position, render_frame};
use tile_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use tile_runtime::runtime::event::RuntimeEvent;
use tile_runtime::runtime::state::Runtime;

use crate::keys::{decode_key, KeyAction};

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

    // Restore the terminal on any exit — normal, error, or panic.
    let cleanup = TerminalCleanupGuard::new();
    cleanup.register_cleanup(Box::new(|| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }));
    let _panic_guard = install_panic_hook(&cleanup);
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;

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

    // Run the loop, then kill any surviving child on EVERY exit path — normal,
    // I/O error, or panic — so no shell outlives the process. A panic is caught,
    // the children are killed, and the panic is re-raised to unwind as usual (the
    // cleanup guard still restores the terminal when `runtime` drops).
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_loop(&mut runtime, &mut terminal, client_id)
    }));
    runtime.kill_all_panes();
    match outcome {
        Ok(result) => result?,
        Err(panic) => resume_unwind(panic),
    }
    Ok(())
}

/// Block on crossterm key events, forwarding each to the inbox as outer input.
/// The quit chord and a read error both send [`RuntimeEvent::Quit`] so the loop
/// stops — a read error means the outer terminal is gone (hangup), which must
/// end the session rather than leave the loop blocked.
fn spawn_input_thread(inbox_tx: mpsc::Sender<RuntimeEvent>, client_id: ClientId) {
    thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(key)) => match decode_key(key) {
                KeyAction::Quit => {
                    let _ = inbox_tx.send(RuntimeEvent::Quit);
                    break;
                }
                KeyAction::Bytes(bytes) => {
                    if inbox_tx
                        .send(RuntimeEvent::OuterInput { client_id, bytes })
                        .is_err()
                    {
                        break;
                    }
                }
                KeyAction::Ignore => {}
            },
            // Resize/mouse/focus/paste are not handled in this slice.
            Ok(_) => {}
            Err(_) => {
                let _ = inbox_tx.send(RuntimeEvent::Quit);
                break;
            }
        }
    });
}

/// The event loop: block until an event is due (bounded by the next render
/// deadline), apply it and any others already queued, repaint if due, and stop
/// once a [`RuntimeEvent::Quit`] arrives or no pane remains. Generic over the
/// backend so a test can drive it headlessly.
fn run_loop<B: Backend>(
    runtime: &mut Runtime,
    terminal: &mut Terminal<B>,
    client_id: ClientId,
) -> Result<(), B::Error> {
    loop {
        let next = runtime.next_render_wakeup(Instant::now());
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
        if quit {
            break;
        }
        if runtime.poll_render(Instant::now()) {
            render(terminal, runtime, client_id)?;
        }
        if !runtime.has_active_panes() {
            break;
        }
    }
    Ok(())
}

/// Route one inbox event to its runtime handler. Returns
/// [`ControlFlow::Break`] when the event is a quit request, so the loop stops.
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
        RuntimeEvent::OuterInput { client_id, bytes } => {
            runtime.handle_outer_input(client_id, &bytes);
        }
        // Timer drives time-based refreshes; resize/IPC/plugin are not wired in
        // this slice.
        RuntimeEvent::Resize { .. }
        | RuntimeEvent::Timer
        | RuntimeEvent::Ipc(_)
        | RuntimeEvent::Plugin(_) => {}
    }
    ControlFlow::Continue(())
}

/// Paint one frame for `client_id`'s viewport, placing the hardware cursor.
/// Generic over the backend so a test can render into an in-memory buffer.
fn render<B: Backend>(
    terminal: &mut Terminal<B>,
    runtime: &Runtime,
    client_id: ClientId,
) -> Result<(), B::Error> {
    let Some(snapshot) = runtime.build_snapshot(client_id) else {
        return Ok(());
    };
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(SnapshotWidget(&snapshot), area);
        if let Some(position) = cursor_position(&snapshot, area) {
            frame.set_cursor_position(position);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
