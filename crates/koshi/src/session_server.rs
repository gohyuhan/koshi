//! The per-session server process: it owns one session's panes and PTYs and
//! answers that session's control socket.
//!
//! It runs with no terminal of its own. Startup reads `koshi.kdl`, installs
//! this session's log subscriber, builds the server, seeds the session under
//! the id and name it was started with, binds the control socket, and prints
//! one JSON line saying where that socket is — the only thing this process
//! ever writes to standard output. Then it serves the runtime inbox — applying
//! each event, timing renders, and handing every attached client its frame —
//! until a `core:quit` command arrives or the last pane's child exits, and
//! tears down.
//!
//! Where that control socket is bound depends on who may reach it: this user's
//! private runtime directory on its own, or the machine-wide shared directory
//! when `koshi.kdl`'s `allow-other-users` is on or `--allow-other-users` forces
//! it on for this session.

use std::io::Write;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Instant, SystemTime};

use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_ipc::router::{SessionServerReady, ROUTER_PROTOCOL_VERSION};
use koshi_observability::logging::init_tracing;
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::portable::PortablePtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_forward::InboxSink;
use koshi_runtime::server::Server;

#[cfg(test)]
mod tests;

/// The size the session's first pane starts at. No client is attached yet, so
/// there is no terminal to read a size from; the first attach resizes it.
const STARTING_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// Run one session to its end: seed it under `session_id` and `session_name`,
/// serve its control socket inside `runtime_dir`, report readiness on standard
/// output, then loop until the session ends.
///
/// The ready line is printed only once the session is seeded and the socket is
/// bound. Any failure before that returns `Err` having printed nothing, so a
/// caller reading standard output sees end of stream and knows the session
/// never started.
///
/// `allow_other_users_override` is the `--allow-other-users` flag the router
/// passes on: `Some(true)` serves the other users of this machine whatever
/// `koshi.kdl` says, and `None` leaves that answer to the file.
pub fn run_session_server(
    runtime_dir: &Path,
    session_id: SessionId,
    session_name: String,
    profile: Option<&str>,
    allow_other_users_override: Option<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = crate::config::load_app_layer();
    let params = crate::config::logging_params(app.as_ref(), session_id);
    let (level, format) = (params.level, params.format);
    let _ = init_tracing(params);
    // The first line written, so a log file that exists at all already says
    // which level and format the session ran under.
    tracing::info!(
        session_id = %session_id,
        level = ?level,
        format = ?format,
        "logging started"
    );

    // This session's server: panes deliver their child's output straight into
    // this inbox from their own PTY reader threads.
    let (inbox_tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let pty_sink: Arc<dyn PtySink> = Arc::new(InboxSink::new(inbox_tx.clone()));
    let backend: Arc<dyn PtyBackend> = Arc::new(PortablePtyBackend::with_sink(pty_sink));
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let mut server = Server::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
    );
    // Read before the layer is handed over, which consumes it.
    let other_users = crate::config::other_users_policy(app.as_ref(), allow_other_users_override);
    server.load_startup_config(app);

    // No client is minted here: this process serves whoever attaches over the
    // control socket, and until one does the session holds none. A profile that
    // will not launch falls back to one shell, so the session always comes up.
    let now = SystemTime::now();
    let template = profile.and_then(crate::config::load_profile);
    let seeded = match template {
        // The name is the router's, not a fresh one: the router registered this
        // session under it and a `koshi attach <name>` resolves against it.
        Some(template) => match server.bootstrap_profile_named(
            session_id,
            session_name.clone(),
            template,
            STARTING_VIEWPORT,
            now,
            None,
        ) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(%err, "profile could not launch; starting a single shell");
                false
            }
        },
        None => false,
    };
    if !seeded {
        server.bootstrap_session(session_id, session_name, STARTING_VIEWPORT, now, None)?;
    }

    // Binds the socket and writes the endpoint file advertising it; the
    // address it reports is the one the ready line carries.
    let ipc_server = IpcServer::start(runtime_dir, session_id, inbox_tx, other_users)?;
    let ready = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket: ipc_server.addr().to_string(),
    };
    server.attach_ipc_server(ipc_server);

    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;

    serve(&mut server);
    server.shutdown();
    Ok(())
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a [`RuntimeEvent::Quit`] arrives, a `core:quit`
/// command is applied, or no pane is left running.
///
/// Serving the inbox is what makes the control socket work: a command
/// forwarded over it and a discovery query asking what this session holds both
/// arrive here as events.
///
/// This process paints nothing itself; the frames it builds go out over the
/// socket to the clients attached to it.
fn serve(server: &mut Server) {
    loop {
        let now = Instant::now();
        let event = match server.next_render_wakeup(now) {
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
            quit |= server.handle_runtime_event(event).is_break();
        }
        // Apply anything else already queued before building one frame.
        while let Ok(event) = server.inbox_rx().try_recv() {
            quit |= server.handle_runtime_event(event).is_break();
        }
        // A subscriber that lost a critical event is paused until it is handed
        // a fresh snapshot; queue that snapshot now so it is applied in this
        // pass and the frame pushed below is built from it.
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
        if quit || server.quit_requested() || !server.has_active_panes() {
            break;
        }
    }
}
