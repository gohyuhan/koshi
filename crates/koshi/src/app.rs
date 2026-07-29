//! The runnable `koshi` binary: terminal setup, genesis, and the event loop.
//!
//! Startup reads the config, installs the log subscriber the config asks for,
//! then enters raw mode + the alternate screen + mouse capture (all restored
//! on drop or panic by a cleanup guard), builds the server, and seeds one
//! session/tab/shell pane. A background thread turns crossterm key and mouse
//! events into inbox events; the main loop drains the inbox, applies each event
//! to the server, and repaints when the render scheduler says a frame is due.
//! Ctrl-Q, or the shell exiting, ends the loop.

use std::collections::VecDeque;
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

use koshi_client::input::KeyOutcome;
use koshi_client::mouse::MouseAction;
use koshi_client::Client;
use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, SessionId};
use koshi_core::key::KeySequence;
use koshi_core::mouse::MouseKind;
use koshi_input::mouse::decode_mouse;
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_observability::logging::{init_tracing, LoggingParams};
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::portable::PortablePtyBackend;
use koshi_renderer::snapshot::{
    CursorStyle, KeymapHints, MouseFrame, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{cursor_position, cursor_style, render_frame};
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_forward::InboxSink;
use koshi_runtime::server::Server;
use koshi_terminal::state::CursorShape;

use crate::keys::decode_key;

/// Paints a render snapshot into ratatui's frame buffer via the widget trait —
/// the only way to reach the frame's buffer, and exactly the shape
/// [`render_frame`] expects.
struct SnapshotWidget<'a> {
    /// The frame the session handed out.
    snapshot: &'a RenderSnapshot,
    /// The colors this viewer paints koshi's chrome in.
    theme: &'a Theme,
    /// The hint-bar data for the mode this viewer is in.
    hints: &'a KeymapHints,
    /// The multi-chord sequence this viewer has open.
    pending: Option<&'a KeySequence>,
    /// The pane this viewer's pointer is over, and where its tab strip sits.
    viewer: ViewerChrome,
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
    // Panes deliver their child's output straight into this inbox from their own
    // PTY reader threads. Handing the backend the sink is what keeps a session's
    // thread count flat as panes are added: without it every pane also needs a
    // thread whose whole job is moving chunks from the pane's channel to here.
    let pty_sink: Arc<dyn PtySink> = Arc::new(InboxSink::new(inbox_tx.clone()));
    let backend: Arc<dyn PtyBackend> = Arc::new(PortablePtyBackend::with_sink(pty_sink));
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let mut server = session(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
        loaded.app.clone(),
    );

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

    // Serve the control socket and advertise it with the endpoint file, so a
    // `koshi` CLI in a second process can reach this session. The socket is a
    // convenience on top of a working terminal: failing to serve it is logged
    // and the session runs on without one.
    match koshi_paths::runtime_dir() {
        Some(runtime_dir) => match IpcServer::start(&runtime_dir, session_id, inbox_tx.clone()) {
            Ok(ipc_server) => {
                tracing::info!(addr = ipc_server.addr(), "control socket serving");
                server.attach_ipc_server(ipc_server);
            }
            Err(error) => {
                tracing::warn!(%error, "control socket unavailable; session commands from other processes will not work");
            }
        },
        None => tracing::warn!(
            "no runtime directory found; session commands from other processes will not work"
        ),
    }

    // The client half: the view side of the process. It subscribes to the
    // server's events under its own client id, so a subscriber that falls
    // behind can be handed a fresh snapshot of that client's view, and holds
    // the cleanup guard, since the outer terminal it restores is the client's.
    let events_rx = server.subscribe(client_id, EventFilter::All);
    let mut client = viewer(client_id, viewport, events_rx, cleanup, loaded);

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

/// Build the session half and apply `app`, the parsed `koshi.kdl` layer, in one
/// step.
///
/// Runs before genesis, so the first session already sees the configured shell
/// and pane floor. `app` is `None` when the file is absent or failed to load.
/// The colors, the keymap, and the split direction new panes open in are each
/// viewer's own; [`viewer`] applies those.
fn session(
    pty_backend: Arc<dyn PtyBackend>,
    snapshot_provider: Arc<dyn SnapshotProvider>,
    storage: Arc<dyn Storage>,
    inbox_rx: mpsc::Receiver<RuntimeEvent>,
    inbox_tx: mpsc::Sender<RuntimeEvent>,
    app: Option<koshi_config::layer::PartialKoshiConfig>,
) -> Server {
    let mut server = Server::new(pty_backend, snapshot_provider, storage, inbox_rx, inbox_tx);
    server.load_startup_config(app);
    server
}

/// Build the viewer half and apply `loaded`'s viewer-owned files, in one step.
///
/// It folds its own settings, resolves its chrome colors, and validates its own
/// keymap, so the palette a frame is painted in and the keys it answers are
/// this terminal's. `keybinding.kdl` is the one file that can be read and then
/// refused, so it is the one whose outcome is logged; app settings and the
/// theme are typed values that always apply.
fn viewer(
    client_id: ClientId,
    viewport: Size,
    events: mpsc::Receiver<koshi_renderer::snapshot::Delivery>,
    cleanup: TerminalCleanupGuard,
    loaded: crate::config::LoadedConfig,
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

/// The event loop: block until an event is due (bounded by the next render
/// deadline), apply it and any others already queued, hand a fresh snapshot to
/// any subscriber that lost a critical event, repaint if due, and stop once a
/// `core:quit` binding fires, a [`RuntimeEvent::Quit`] (terminal hangup)
/// arrives, or no pane remains. Generic over the backend so a test can drive
/// it headlessly.
fn run_loop<B: Backend>(
    server: &mut Server,
    client: &mut Client,
    terminal: &mut Terminal<B>,
) -> Result<(), B::Error> {
    let mut last_title = String::new();
    let mut last_cursor = None;
    // The frame the viewer is looking at, cut down to what a wheel tick reads:
    // no cells, so nothing here keeps a pane's grid alive between paints. It is
    // replaced at each paint. Before the first paint there is none, and a wheel
    // tick arriving in that window is dropped.
    let mut last_frame: Option<MouseFrame> = None;
    loop {
        let now = Instant::now();
        let next = earliest(
            earliest(server.next_render_wakeup(now), client.next_key_wakeup(now)),
            client.next_mouse_wakeup(now),
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
            quit |= apply_event(server, client, event, last_frame.as_ref()).is_break();
        }
        // Apply anything else already queued before painting one frame.
        while let Ok(event) = server.inbox_rx().try_recv() {
            quit |= apply_event(server, client, event, last_frame.as_ref()).is_break();
        }
        // A subscriber that lost a critical event is paused until it is handed
        // a fresh snapshot; queue that snapshot now so it is applied in this
        // pass and the frame painted below is drawn from it.
        server.resync_lagged();
        // Everything the subscription delivered that no key press already took:
        // a mode change asked for from outside — `koshi lock --client` — lands
        // here, so it is in the mode the ambiguity deadline below is read in
        // and in the frame painted after it. It also empties the bounded queue
        // on a batch with no keys in it.
        client.apply_events();
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
        fire_expired_key_sequence(server, client, Instant::now());
        // A selection drag held past a pane's edge keeps scrolling while the
        // pointer sits still, so the clock drives it.
        if let Some(frame) = last_frame.as_ref() {
            let actions = client.expire_mouse_scroll(Instant::now(), frame);
            apply_mouse_actions(server, client, frame, actions);
        }
        if server.poll_render(Instant::now()) {
            render(
                terminal,
                server,
                client,
                &mut last_title,
                &mut last_cursor,
                &mut last_frame,
            )?;
        }
        if !server.has_active_panes() {
            break;
        }
    }
    Ok(())
}

/// Fire the viewer's open key sequence if its ambiguity deadline has passed at
/// `now`.
///
/// A sequence that is both a complete binding and a longer one's prefix fires
/// when its deadline passes. The viewer holds it, so it decides; the session
/// only runs what comes back — with this viewer's own default split side, the
/// same one its key presses are answered with.
///
/// The action's report of what it changed about this viewer is taken back
/// straight away: a `core:lock` fired here publishes the new input mode as it
/// runs, after the batch's own take, so without this the hint bar would be
/// painted in the mode the viewer just left.
fn fire_expired_key_sequence(server: &mut Server, client: &mut Client, now: Instant) {
    let Some(bound) = client.expire_key_sequence(now) else {
        return;
    };
    let direction = client.config().layout.new_pane_direction;
    server.handle_bound_action(client.id(), bound, direction);
    server.invalidate_status();
    client.apply_events();
}

/// Hand one inbox event to the server, after the client has taken the parts
/// that are its own: a key it must resolve, a wheel tick it must route, its
/// terminal's new size, and the end of a selection gesture its own paste key
/// finished. `last_frame` is the frame the viewer is looking at, or
/// `None` before the first paint. Returns [`ControlFlow::Break`] when the event
/// is a quit request, so the loop stops.
fn apply_event(
    server: &mut Server,
    client: &mut Client,
    event: RuntimeEvent,
    last_frame: Option<&MouseFrame>,
) -> ControlFlow<()> {
    // A key belongs to the viewer that received it: the keymap, the input mode
    // and any open sequence all live there, so it decides what the press means
    // and the session only ever sees the answer — a resolved action, or a
    // press to write.
    if let RuntimeEvent::KeyInput { client_id, chord } = event {
        if client_id == client.id() {
            // The mode this key is read in must be the mode the session last
            // reported. A `core:lock` earlier in this same batch published the
            // change into the subscription as it ran, so taking it now is what
            // makes the very next key see the new mode.
            client.apply_events();
            match client.resolve_key(chord, Instant::now()) {
                KeyOutcome::Fire(bound) => {
                    let direction = client.config().layout.new_pane_direction;
                    server.handle_bound_action(client_id, bound, direction);
                }
                KeyOutcome::PassThrough(chord) => {
                    // The key belongs to the program in the pane, so a selection
                    // gesture over it is over.
                    client.end_mouse_selection();
                    server.handle_key_press(client_id, chord);
                }
                // Held or dropped: nothing reaches the session, but the hint
                // bar and mode tag are drawn from viewer state, so the frame
                // is stale either way.
                KeyOutcome::Pending | KeyOutcome::Discard => {}
            }
            server.invalidate_status();
            return ControlFlow::Continue(());
        }
    }
    // A mouse event belongs to the viewer too: the frame it painted says which
    // pane the pointer is over, what that pane's program asked for, and which
    // gesture is under way, and its own `mouse` and `copy` settings say what
    // each of those means. The session only runs what comes back.
    if let RuntimeEvent::MouseInput { client_id, mouse } = event {
        if client_id == client.id() {
            if let Some(frame) = last_frame {
                // The mode this event is routed in must be the mode the
                // session last reported. A `core:mouse-select` earlier in this
                // same batch published its change into the subscription as it
                // ran, so taking it now is what makes this event route the new
                // way.
                client.apply_events();
                let tab = frame.client.active_tab;
                let before = client.chrome(tab);
                let actions = client.handle_mouse(mouse, frame, Instant::now());
                apply_mouse_actions(server, client, frame, actions);
                // The hovered pane and the tab strip's position are painted from
                // viewer state, which no session mutation marks stale.
                if client.chrome(tab) != before {
                    server.invalidate_status();
                }
            }
            return ControlFlow::Continue(());
        }
    }
    if let RuntimeEvent::Resize { client_id, size } = &event {
        if *client_id == client.id() {
            client.set_viewport(*size);
        }
    }
    // The user pressed their terminal's paste key, so the text is theirs and it
    // goes to the program in the pane: a selection gesture over it is over. Only
    // this viewer's own paste ends it — a write arriving from anywhere else does
    // not touch the gesture.
    if let RuntimeEvent::HostPaste { client_id, .. } = &event {
        if *client_id == client.id() {
            client.end_mouse_selection();
        }
    }
    server.handle_runtime_event(event)
}

/// Ask the session to run everything the viewer decided one mouse event means,
/// in the order it decided them. `frame` is the frame the viewer decided
/// against, which a scroll's answer is re-measured over.
fn apply_mouse_actions(
    server: &mut Server,
    client: &mut Client,
    frame: &MouseFrame,
    actions: Vec<MouseAction>,
) {
    let client_id = client.id();
    let mut queue: VecDeque<MouseAction> = actions.into();
    while let Some(action) = queue.pop_front() {
        match action {
            MouseAction::Scroll { pane, up, lines } => {
                let top = server.scroll_pane_view(client_id, pane, up, lines);
                queue.extend(client.note_scroll_applied(pane, top, frame));
            }
            MouseAction::Forward { pane, mouse } => {
                let written = server.forward_mouse_to_pane(client_id, pane, mouse);
                // A gesture is captured once the pane's program has seen the
                // press that began it.
                if let (true, MouseKind::Press(button)) = (written, mouse.kind) {
                    client.note_press_forwarded(pane, button);
                }
            }
            MouseAction::AltScrollArrows { pane, up, count } => {
                server.write_alt_scroll_arrows(pane, up, count);
            }
            MouseAction::Resize {
                pane,
                side,
                step,
                count,
            } => {
                let applied = server.drag_resize(client_id, pane, side, step, count);
                client.note_resize_applied(applied);
            }
            MouseAction::Command(command) => {
                let envelope = CommandEnvelope::new(
                    CommandId::new(),
                    CommandSource::mouse(client_id),
                    SystemTime::now(),
                    command,
                );
                let _ = server.submit_command(envelope);
            }
        }
    }
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
///
/// A [`MouseFrame`] of what it painted is left in `last_frame`, which is what
/// the viewer answers a wheel tick from. Painting is also how the viewer learns
/// which tab it is on, so a tab-strip peek made on another tab is thrown away
/// here.
fn render<B: Backend>(
    terminal: &mut Terminal<B>,
    server: &Server,
    client: &mut Client,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
    last_frame: &mut Option<MouseFrame>,
) -> Result<(), B::Error> {
    let Some(snapshot) = server.build_snapshot(client.id()) else {
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
    // The hint bar is drawn from the viewer's own keymap and its own mode; the
    // one thing it takes from the frame is whether mouse-select is on, which
    // decides the label that entry wears.
    let hints = client.frame_hints(snapshot.client.mouse_select);
    // The viewer now sees which tab it is on, whatever moved it there — a click,
    // a keybinding, an IPC command, a closed tab. A tab-strip peek made on
    // another tab is thrown away here, so switching back to that tab starts from
    // it rather than from the peek.
    client.note_active_tab(snapshot.client.active_tab);
    // The hovered pane and the tab strip's position are the viewer's own, so the
    // frame the session handed out says nothing about either.
    let viewer = client.chrome(snapshot.client.active_tab);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            SnapshotWidget {
                snapshot: &snapshot,
                theme: client.theme(),
                hints: &hints,
                pending: client.pending_sequence(),
                viewer,
            },
            area,
        );
        if let Some(position) = cursor_position(&snapshot, area) {
            frame.set_cursor_position(position);
        }
    })?;
    // The mouse-sized part of what was just painted. Everything the mouse does
    // not read — every pane's grid handle, cursor, and title — is dropped here.
    *last_frame = Some(MouseFrame::from(snapshot));
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
