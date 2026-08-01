//! The per-session server process: it owns one session's panes and PTYs and
//! answers that session's control socket.
//!
//! It runs with no terminal of its own. Startup reads `koshi.kdl`, builds the
//! server, seeds the session under the id and name it was started with, binds
//! the control socket, and prints one JSON line saying where that socket is —
//! the only thing this process ever writes to standard output. Then it drains
//! the runtime inbox until a `core:quit` command arrives or the last pane's
//! child exits, and tears down.

use std::io::Write;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_ipc::router::{SessionServerReady, ROUTER_PROTOCOL_VERSION};
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
pub fn run_session_server(
    runtime_dir: &Path,
    session_id: SessionId,
    session_name: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = crate::config::load_app_layer();

    // The same server construction the interactive launch uses: panes deliver
    // their child's output straight into this inbox from their own PTY reader
    // threads.
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
    server.load_startup_config(app);

    server.bootstrap_local_named(
        session_id,
        session_name,
        STARTING_VIEWPORT,
        SystemTime::now(),
    )?;

    // Binds the socket and writes the endpoint file advertising it; the
    // address it reports is the one the ready line carries.
    let ipc_server = IpcServer::start(runtime_dir, session_id, inbox_tx)?;
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

/// Drain the runtime inbox one event at a time until the session ends: the
/// inbox loses its last sender, a [`RuntimeEvent::Quit`] arrives, a `core:quit`
/// command is applied, or no pane is left running.
///
/// Serving the inbox is what makes the control socket work: a command
/// forwarded over it and a discovery query asking what this session holds both
/// arrive here as events.
fn serve(server: &mut Server) {
    loop {
        let Ok(event) = server.inbox_rx().recv() else {
            break;
        };
        if server.handle_runtime_event(event).is_break() {
            break;
        }
        if server.quit_requested() || !server.has_active_panes() {
            break;
        }
    }
}
