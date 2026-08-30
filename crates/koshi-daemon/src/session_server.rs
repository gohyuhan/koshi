//! The per-session server process: it owns one session's panes and PTYs and
//! answers that session's control socket.
//!
//! It runs with no terminal of its own. Startup reads `koshi.kdl`, installs
//! this session's log subscriber, builds the server, seeds the session under
//! the id and name it was started with, binds the control socket, and prints
//! one JSON line saying where that socket is — the only thing this process
//! ever writes to standard output. Then it serves the runtime inbox — applying
//! each event, timing renders, and handing every attached client its frame —
//! until the last pane's child exits, or a `core:quit` command is applied and
//! no client is still expected back from an image swap, and tears down.
//!
//! Where that control socket is bound depends on who may reach it: this user's
//! private runtime directory on its own, or the machine-wide shared directory
//! when `koshi.kdl`'s `allow-other-users` is on or `--allow-other-users` forces
//! it on for this session.
//!
//! ## Replacing its own image
//!
//! A restart request accepted over the control socket ends the serve loop into
//! the swap. The swap holds every pane's reader still, tells every attached
//! client to come back, writes the session's whole state to the resume file
//! beside the endpoint file, withdraws the control socket, and replaces this
//! process's image with the binary on disk. On Unix that is `execvp`, which
//! keeps the process id and every pane's terminal descriptor; on Windows it
//! starts the new image and ends, and the panes live in a helper process that
//! outlives both. The new image is started with `--resume`, takes every pane
//! back, rebuilds the session from the carried state, binds a fresh socket, and
//! deletes the file — and deletes it just the same when it cannot come up at
//! all. A new image that never starts leaves the file behind, and the router
//! removes it once it is older than
//! [`RESTART_WINDOW`](koshi_ipc::endpoint::RESTART_WINDOW).
//!
//! Nothing irreversible happens before the panes are held still, so a swap that
//! cannot start leaves the session serving in this process with every pane and
//! every reader running.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_config::layer::PartialKoshiConfig;
use koshi_core::geometry::Size;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{KillPolicy, PtySize};
use koshi_ipc::endpoint::{resume_path, RESTART_WINDOW};
use koshi_ipc::error::IpcError;
use koshi_ipc::router::{SessionServerReady, ROUTER_PROTOCOL_VERSION};
use koshi_observability::logging::init_tracing;
use koshi_pty::backend::state::{PtyBackend, PtyHandle, PtySink};
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage};
use koshi_runtime::resume::{self, ResumeBody, ResumeHeader, RESUME_FORMAT, RESUME_FORMAT_MIN};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_forward::InboxSink;
use koshi_runtime::server::{binary_is_runnable, panes_can_be_carried, RestartCheck, Server};
use koshi_storage::error::StorageError;
use serde::{Deserialize, Serialize};

use koshi_link::router_client::RUNTIME_DIR_FLAG;

#[cfg(unix)]
use koshi_pty::kill::PtyChildKillControl;
#[cfg(unix)]
use koshi_pty::portable::{set_terminal_cloexec, terminal_master_name, PortablePtyBackend};

#[cfg(windows)]
use koshi_ipc::protocol::ConnectionToken;
#[cfg(windows)]
use koshi_ipc::supervisor::supervisor_socket_addr;
#[cfg(windows)]
use koshi_pty::supervisor::SupervisorPtyBackend;

#[cfg(test)]
mod tests;

/// The size the session's first pane starts at. No client is attached yet, so
/// there is no terminal to read a size from; the first attach resizes it.
const STARTING_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The subcommand a session server runs the newly installed binary under to
/// read which resume-file formats it can take back. It prints one JSON line and
/// exits.
pub const RESUME_SUPPORT_SUBCOMMAND: &str = "resume-support";

/// The flag carrying the carried state to the image replacing this one. The
/// file it names is read once and then removed.
const RESUME_FLAG: &str = "--resume";

/// The flag carrying the secret of the link to the process holding this
/// session's panes. Windows only: on Unix the panes are the session server's
/// own children and there is no link.
const SUPERVISOR_TOKEN_FLAG: &str = "--supervisor-token";

/// The flag carrying the process id of the process holding this session's
/// panes. That id is part of the link's address, so the image replacing this
/// one needs it to reach the same panes. Passed on beside
/// [`SUPERVISOR_TOKEN_FLAG`], and Windows only for the same reason.
const SUPERVISOR_PID_FLAG: &str = "--supervisor-pid";

/// How long a client whose record came across an image swap has to attach
/// again. A record nobody claims when the window closes is detached.
const RECONNECT_GRACE: Duration = Duration::from_secs(30);

/// How long the newly installed binary has to say which resume formats it
/// reads. One that has not answered by then is refused, so a binary that never
/// exits cannot hold the thread serving the session.
const RESUME_SUPPORT_WAIT: Duration = Duration::from_secs(5);

/// How long a session server waits for the process holding its panes to start
/// listening.
#[cfg(windows)]
const SUPERVISOR_LINK_WAIT: Duration = Duration::from_secs(10);

/// How long the wait for the process holding the panes pauses between attempts.
#[cfg(windows)]
const SUPERVISOR_LINK_POLL: Duration = Duration::from_millis(50);

/// How long a swap waits for every told client to send its `Leaving` request.
/// The connections still open when it passes are closed.
const CLIENTS_LEFT_LIMIT: Duration = Duration::from_secs(1);

/// How long the wait for the told clients pauses between passes over the
/// runtime inbox.
const CLIENTS_LEFT_POLL: Duration = Duration::from_millis(2);

/// The backend this session server drives its panes through.
///
/// On Unix every pane is this process's own child on this process's own
/// backend: `execvp` keeps the process id, so the children keep their parent
/// and their terminals across a swap. On Windows a pane's pseudoconsole cannot
/// leave the process that opened it, so the panes live in a helper process and
/// this backend is the link to it.
///
/// The swap reaches `pause_readers`, `resume_readers`, `flush_writers` and
/// `carried_panes` through this concrete type, so the session server keeps it
/// beside the `Arc<dyn PtyBackend>` the server holds.
#[cfg(unix)]
type PtyOwner = PortablePtyBackend;

/// The backend this session server drives its panes through. See the Unix
/// definition.
#[cfg(windows)]
type PtyOwner = SupervisorPtyBackend;

/// The one line `koshi resume-support` prints: which resume-file formats that
/// build takes back.
///
/// A session server asks the newly installed binary this before it does
/// anything it cannot undo: the install already replaced the old binary on
/// disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSupport {
    /// The oldest resume-file format this build reads.
    pub min: u32,
    /// The newest resume-file format this build reads.
    pub max: u32,
}

impl ResumeSupport {
    /// What this build reads.
    #[must_use]
    pub fn of_this_build() -> ResumeSupport {
        ResumeSupport {
            min: RESUME_FORMAT_MIN,
            max: RESUME_FORMAT,
        }
    }
}

/// What this session server was started with: the identity the router gave it,
/// where it serves, and how the image that replaces it is started.
///
/// Every argument here is passed on to that image, so the resumed session comes
/// up under the same id and name, in the same directory, under the same
/// `--allow-other-users` flag. The profile is absent: it opened this session's
/// tabs and panes once, and the carried state is what brings them back.
struct SessionStart {
    /// The directory this session serves in.
    runtime_dir: PathBuf,
    /// The session's id, which the router picked.
    session_id: SessionId,
    /// The session's display name, which the router generated.
    session_name: String,
    /// Whether `--allow-other-users` was on this process's command line. It
    /// forces the socket's reach on whatever `koshi.kdl` says, so it is passed
    /// on and the rebound socket stays reachable by the same users. With the
    /// flag off, the rebound socket takes the reach `koshi.kdl` holds at that
    /// moment.
    allow_other_users: bool,
    /// The path this program was started from. A swap runs the binary there.
    exe: PathBuf,
    /// The secret the link to the process holding the panes presents at Hello.
    /// `None` on Unix, where the panes are this process's own children.
    supervisor_token: Option<String>,
    /// The process id of that same process, which its link address is derived
    /// from. `None` on Unix, and set together with `supervisor_token` on
    /// Windows.
    supervisor_pid: Option<u32>,
}

/// Why the serve loop ended.
#[derive(Debug, PartialEq, Eq)]
enum ServeOutcome {
    /// The session is over, on the terms [`serve`] states.
    Ended,
    /// A restart request was accepted, so this process replaces its own image.
    Restart,
}

/// Run one session to its end: seed it under `session_id` and `session_name`,
/// serve its control socket inside `runtime_dir`, report readiness on standard
/// output, then loop until the session ends.
///
/// The ready line is printed only once the session is seeded and the socket is
/// bound. Any failure before that returns `Err` having printed nothing, so a
/// caller reading standard output sees end of stream and knows the session
/// never started.
///
/// `profile` names the profile the session opens its tabs and panes from.
/// `None`, a name no profile file answers to, and a profile that will not
/// launch each open one shell instead.
///
/// `allow_other_users_override` is the `--allow-other-users` flag the router
/// passes on: `Some(true)` serves the other users of this machine whatever
/// `koshi.kdl` says, and `None` leaves that answer to the file.
///
/// `resume_from` is the `--resume` flag the image being replaced passes on: the
/// carried state this session comes up from instead of being seeded. A resume
/// run reads no profile, since the carried state already holds the tabs and
/// panes the profile opened. `supervisor_token` and `supervisor_pid` are the
/// `--supervisor-token` and `--supervisor-pid` flags that go with it on
/// Windows, naming the secret the link to the process holding the panes
/// presents and the process id its address is derived from.
#[allow(clippy::too_many_arguments)]
pub fn run_session_server(
    runtime_dir: &Path,
    session_id: SessionId,
    session_name: String,
    profile: Option<&str>,
    allow_other_users_override: Option<bool>,
    resume_from: Option<&Path>,
    supervisor_token: Option<&str>,
    supervisor_pid: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = koshi_link::config::load_app_layer();
    let params = koshi_link::config::logging_params(app.as_ref(), session_id);
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

    let mut start = SessionStart {
        runtime_dir: runtime_dir.to_path_buf(),
        session_id,
        session_name: session_name.clone(),
        allow_other_users: allow_other_users_override == Some(true),
        exe: std::env::current_exe()?,
        supervisor_token: supervisor_token.map(str::to_string),
        supervisor_pid,
    };

    // This session's server: panes deliver their child's output straight into
    // this inbox from their own PTY reader threads.
    let (inbox_tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let pty_sink: Arc<dyn PtySink> = Arc::new(InboxSink::new(inbox_tx.clone()));

    let (mut server, panes, mut ipc_server) = match resume_from {
        Some(resume_file) => resume_from_file(
            resume_file,
            &mut start,
            app,
            Arc::clone(&pty_sink),
            inbox_rx,
            &inbox_tx,
        )?,
        None => seed_new_session(
            &mut start,
            profile,
            app,
            Arc::clone(&pty_sink),
            inbox_rx,
            &inbox_tx,
        )?,
    };
    install_restart_check(&mut server, &panes, &start.exe);

    report_ready(&ipc_server, resume_from.is_some())?;

    loop {
        match serve(&mut server) {
            ServeOutcome::Ended => break,
            ServeOutcome::Restart => match swap(server, ipc_server, &panes, &start, &inbox_tx)? {
                // The session runs in another process from here; this one ends
                // without touching a single pane.
                None => return Ok(()),
                Some((kept, socket)) => {
                    server = kept;
                    ipc_server = socket;
                    install_restart_check(&mut server, &panes, &start.exe);
                }
            },
        }
    }

    // Every attached client is told the session ended, and holds that frame,
    // before anything is torn down: nothing else joins the threads writing to
    // the clients.
    server.announce_quit();

    // The socket stops before the panes are killed, so nothing advertises a
    // session that is ending.
    ipc_server.shutdown();
    server.shutdown();
    Ok(())
}

/// Seed the one session this process serves and bind its control socket.
///
/// No client is minted here: this process serves whoever attaches over the
/// control socket, and until one does the session holds none. A profile that
/// will not launch falls back to one shell, so the session always comes up.
fn seed_new_session(
    start: &mut SessionStart,
    profile: Option<&str>,
    app: Option<PartialKoshiConfig>,
    sink: Arc<dyn PtySink>,
    inbox_rx: Receiver<RuntimeEvent>,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let (mut server, panes) = server_over_new_panes(start, app, sink, inbox_rx, inbox_tx)?;

    let now = SystemTime::now();
    let template = profile.and_then(koshi_link::config::load_profile);
    let seeded = match template {
        // The name is the router's, not a fresh one: the router registered this
        // session under it and a `koshi attach <name>` resolves against it.
        Some(template) => match server.bootstrap_profile_named(
            start.session_id,
            start.session_name.clone(),
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
        server.bootstrap_session(
            start.session_id,
            start.session_name.clone(),
            STARTING_VIEWPORT,
            now,
            None,
        )?;
    }

    let ipc_server = bind_socket(start, inbox_tx)?;
    Ok((server, panes, ipc_server))
}

/// Come up from the state the previous process image carried out.
///
/// A body that reads gives the session back what [`ResumeBody`] carries, over
/// the panes taken back from the header. A body that does not read ends every
/// pane the header names and seeds one fresh shell under the same id and name,
/// so the session id the router registered still answers.
///
/// The previous image let its panes go — on Unix by ending, on Windows by
/// dropping its link — so every pane is taken back from a descriptor and a
/// process id, or from the helper process holding it.
/// [`resume_readers_and_rebuild`] is the other path: it rebuilds the same state
/// over panes that were never released.
///
/// The file is deleted on every way out of this call. It outlives the socket
/// being bound. While it exists, the router leaves this session's
/// advertisement in place through the swap.
fn resume_from_file(
    resume_file: &Path,
    start: &mut SessionStart,
    app: Option<PartialKoshiConfig>,
    sink: Arc<dyn PtySink>,
    inbox_rx: Receiver<RuntimeEvent>,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let (header, raw_body) = match resume::read_header(resume_file) {
        Ok(read) => read,
        Err(error) => {
            // A header that does not read names no pane, so nothing can be
            // taken back and nothing can be ended. The file goes with it.
            let _ = std::fs::remove_file(resume_file);
            return Err(error.into());
        }
    };

    let body = resume::read_body(header.format, &raw_body);
    let built = build_from_carried_state(&header, body, start, app, sink, inbox_rx, inbox_tx);
    // The state is in memory and the socket carries a fresh token, or nothing
    // came up at all; either way the file has done its work.
    let _ = std::fs::remove_file(resume_file);
    built
}

/// Build the server, the panes and the bound socket the carried state names, as
/// [`resume_from_file`] hands them on.
///
/// `body` is what reading the carried body gave. The panes come back only when
/// that read worked and every pane the header names is taken back; either
/// failure ends every pane the header names and seeds one fresh shell instead.
///
/// # Errors
/// Returns the failure of fresh panes that could not be opened, of a fresh
/// session that could not be seeded, and of a control socket that could not be
/// bound.
fn build_from_carried_state(
    header: &ResumeHeader,
    body: Result<ResumeBody, StorageError>,
    start: &mut SessionStart,
    app: Option<PartialKoshiConfig>,
    sink: Arc<dyn PtySink>,
    inbox_rx: Receiver<RuntimeEvent>,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let carried = match body {
        Ok(body) => match take_panes_back(header, Arc::clone(&sink), start) {
            Ok((panes, handles)) => Some((body, panes, handles)),
            Err(error) => {
                tracing::error!(
                    %error,
                    "the carried panes could not be taken back; the session comes back with one shell"
                );
                None
            }
        },
        Err(error) => {
            tracing::error!(
                %error,
                wrote = header.format,
                reads_from = RESUME_FORMAT_MIN,
                reads_to = RESUME_FORMAT,
                "the carried state could not be read; the session comes back with one shell"
            );
            release_carried_panes(header, start, Arc::clone(&sink));
            None
        }
    };

    // Either way the session comes back on the `koshi.kdl` that is on disk now.
    let (server, panes) = match carried {
        Some((body, panes, handles)) => {
            let backend: Arc<dyn PtyBackend> = panes.clone();
            let mut server = Server::resume(
                backend,
                inbox_rx,
                inbox_tx.clone(),
                body,
                handles,
                carried_sizes(header),
            );
            server.load_startup_config(app);
            start_reconnect_deadline(inbox_tx.clone());
            (server, panes)
        }
        None => {
            let (mut server, panes) = server_over_new_panes(start, app, sink, inbox_rx, inbox_tx)?;
            // The identity the router registered, and the one the socket below
            // binds under, so the session id still answers.
            server.bootstrap_session(
                start.session_id,
                start.session_name.clone(),
                STARTING_VIEWPORT,
                SystemTime::now(),
                None,
            )?;
            (server, panes)
        }
    };

    let ipc_server = bind_socket(start, inbox_tx)?;
    Ok((server, panes, ipc_server))
}

/// Open the panes this session runs on.
///
/// On Unix they are this process's own children on its own backend. On Windows
/// they belong to a helper process this starts and outlive an image swap; the
/// secret its link presents and its process id are recorded on
/// `start`, since the image replacing this one needs both to reach the same
/// panes.
///
/// The helper's address carries its process id, so the helper started here
/// never binds the address a helper this session is leaving behind still holds.
///
/// # Errors
/// Returns the failure of a helper process that could not be started or could
/// not be reached, which the caller reports as the session failing to start.
fn open_panes(
    start: &mut SessionStart,
    sink: Arc<dyn PtySink>,
) -> Result<Arc<PtyOwner>, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let _ = start;
        Ok(Arc::new(PortablePtyBackend::with_sink(sink)))
    }
    #[cfg(windows)]
    {
        let token = ConnectionToken::generate();
        let supervisor_pid = crate::pty_supervisor::spawn_pty_supervisor(
            &start.runtime_dir,
            start.session_id,
            &token,
        )?;
        let panes = link_to_supervisor(
            start.session_id,
            supervisor_pid,
            &start.runtime_dir,
            &token,
            sink,
            &[],
        )?;
        start.supervisor_token = Some(token.expose().to_string());
        start.supervisor_pid = Some(supervisor_pid);
        Ok(panes)
    }
}

/// Open fresh panes and build the server driving them, on the `koshi.kdl` that
/// is on disk now.
///
/// The server holds no session yet: the caller seeds one. Two callers reach it
/// — a first run, and a resume whose carried state could not be read.
///
/// # Errors
/// Returns the failure of panes that could not be opened.
fn server_over_new_panes(
    start: &mut SessionStart,
    app: Option<PartialKoshiConfig>,
    sink: Arc<dyn PtySink>,
    inbox_rx: Receiver<RuntimeEvent>,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>), Box<dyn std::error::Error>> {
    let panes = open_panes(start, sink)?;
    let backend: Arc<dyn PtyBackend> = panes.clone();
    let mut server = Server::new(
        backend,
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        inbox_rx,
        inbox_tx.clone(),
    );
    server.load_startup_config(app);
    Ok((server, panes))
}

/// The panes taken back after an image swap: the backend driving them, and one
/// handle per pane for the rebuilt server to hold.
type TakenBackPanes = (Arc<PtyOwner>, HashMap<PaneId, PtyHandle>);

/// Take every pane the header names back, and hand back the backend driving
/// them with one handle each.
///
/// A pane's terminal descriptor crossed the swap open, so each pane is taken
/// back from that descriptor and its child's process id by
/// [`take_one_pane_back`].
///
/// When one pane cannot be taken back, every pane the header names is ended
/// before the failure is returned, so the caller holds nothing half-owned.
///
/// # Errors
/// Returns whatever [`take_one_pane_back`] reports, the sentence naming a pane
/// the carried state names twice, and the sentence naming a descriptor the
/// carried state gives to two panes.
#[cfg(unix)]
fn take_panes_back(
    header: &ResumeHeader,
    sink: Arc<dyn PtySink>,
    _start: &SessionStart,
) -> Result<TakenBackPanes, Box<dyn std::error::Error>> {
    header_names_each_pane_once(header)?;

    let panes = Arc::new(PortablePtyBackend::with_sink(sink));
    let mut handles = HashMap::new();
    // Which pane each descriptor was taken back on, so a number the header
    // names twice is refused rather than owned by two panes and closed twice.
    let mut taken_on: HashMap<i32, PaneId> = HashMap::new();
    for (index, pane) in header.panes.iter().enumerate() {
        if let Some(raw) = pane.terminal_fd {
            if let Some(&earlier) = taken_on.get(&raw) {
                end_panes_after_failure(header, index + 1);
                return Err(format!(
                    "pane {} carried descriptor {raw}, which pane {earlier} was already taken back \
                     on, so it cannot be taken back",
                    pane.pane_id
                )
                .into());
            }
        }
        match take_one_pane_back(&panes, pane) {
            Ok(handle) => {
                handles.insert(pane.pane_id, handle);
                if let Some(raw) = pane.terminal_fd {
                    taken_on.insert(raw, pane.pane_id);
                }
            }
            Err(error) => {
                end_panes_after_failure(header, index + 1);
                return Err(error);
            }
        }
    }
    Ok((panes, handles))
}

/// Refuse a carried state that names one pane more than once.
///
/// Every pane is taken back onto its own entry, keyed by pane id, so each id
/// appears once.
///
/// Before → after: a header naming panes `A`, `B` → each is taken back. A
/// header naming `A`, `A` → the sentence naming `A`, and no pane is touched.
///
/// # Errors
/// Returns the sentence naming the pane the carried state names twice.
fn header_names_each_pane_once(header: &ResumeHeader) -> Result<(), Box<dyn std::error::Error>> {
    let mut named = HashSet::new();
    for pane in &header.panes {
        if !named.insert(pane.pane_id) {
            return Err(format!(
                "pane {} is named twice by the carried state, so it cannot be taken back",
                pane.pane_id
            )
            .into());
        }
    }
    Ok(())
}

/// Take one pane back from the terminal descriptor and process id the header
/// carried, and hand back the handle the rebuilt server holds it by.
///
/// What the number names is read before this process owns it, in two steps.
///
/// 1. A number that names no pseudoterminal master is refused, so a number
///    naming an ordinary file, a pipe, this process's own standard error, or
///    nothing at all never becomes a pane's terminal.
/// 2. A number that names a master is refused when the header recorded which
///    terminal that master is paired with and the descriptor is now paired with
///    another one. A header that recorded no name leaves step 1 to decide.
///
/// Close-on-exec goes back on the descriptor once both steps pass. The exit
/// status the header carried goes to the pane as well, so a child the previous
/// image reaped is reported with the code it really ended with.
///
/// Before → after: the header carries `terminal_fd = 7` and
/// `terminal_name = "/dev/ttys009"` for pane 3, and descriptor 7 is now the
/// master of `/dev/ttys011` → the sentence naming both terminals comes back and
/// descriptor 7 is left alone.
///
/// # Errors
/// Returns the sentence naming a pane the header carried no descriptor for, the
/// sentence naming a descriptor that is no pseudoterminal master, the sentence
/// naming a descriptor whose terminal is not the one the header recorded, the OS
/// error of a descriptor whose flags cannot be set, and the failure of a pane
/// the backend could not take back.
#[cfg(unix)]
fn take_one_pane_back(
    panes: &Arc<PortablePtyBackend>,
    pane: &resume::CarriedPane,
) -> Result<PtyHandle, Box<dyn std::error::Error>> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let raw = pane.terminal_fd.ok_or_else(|| {
        format!(
            "pane {} carried no terminal descriptor, so it cannot be taken back",
            pane.pane_id
        )
    })?;
    let Some(named_now) = terminal_master_name(raw) else {
        return Err(format!(
            "pane {} carried descriptor {raw}, which names no pseudoterminal master, \
             so it cannot be taken back",
            pane.pane_id
        )
        .into());
    };
    if let Some(named_then) = &pane.terminal_name {
        if *named_then != named_now {
            return Err(format!(
                "pane {} carried descriptor {raw} as the master of {named_then}, which is now \
                 the master of {named_now}, so it cannot be taken back",
                pane.pane_id
            )
            .into());
        }
    }
    set_terminal_cloexec(raw, true)?;
    // The descriptor crossed the swap open and names this process's own
    // pseudoterminal master, so it is this process's own from here.
    let terminal = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(panes.adopt(pane.pane_id, terminal, pane.pid, pane.size(), pane.exit)?)
}

/// End every pane the header names after a failure, and close the terminals
/// from `untouched` onward.
///
/// Each child's whole process group is ended, which reaps its grandchildren
/// too. `untouched` is the first pane this process never took over. The panes
/// before it belong to the backend, which closes them as it is dropped, apart
/// from the one whose hand-over failed: its descriptor is left as it is —
/// either the call that failed closed it, or this process never took it over
/// and the number stays as the swap left it until this process ends. An
/// `untouched` of `0` is a failure before any pane was taken back, so every
/// terminal the header names is closed here.
#[cfg(unix)]
fn end_panes_after_failure(header: &ResumeHeader, untouched: usize) {
    for pane in &header.panes {
        let _ = end_carried_child(pane.pid);
    }
    for pane in &header.panes[untouched..] {
        close_carried_terminal(pane);
    }
}

/// Close one carried pane's terminal descriptor.
///
/// What the number names is read first: only a pseudoterminal master is closed,
/// so a number that names an ordinary file, a pipe, a socket or this process's
/// own standard error is left open and logged.
///
/// Before → after: the header carries `terminal_fd = 2` and descriptor 2 is
/// this process's own standard error → descriptor 2 stays open.
#[cfg(unix)]
fn close_carried_terminal(pane: &resume::CarriedPane) {
    use std::os::fd::{FromRawFd, OwnedFd};

    let Some(raw) = pane.terminal_fd else {
        return;
    };
    if terminal_master_name(raw).is_none() {
        tracing::warn!(
            pane = %pane.pane_id,
            terminal_fd = raw,
            "the carried state named a descriptor that is no pseudoterminal master; it stays open"
        );
        return;
    }
    drop(unsafe { OwnedFd::from_raw_fd(raw) });
}

/// Take every pane the header names back by linking to the helper process
/// holding them, and hand back the backend driving them with one handle each.
///
/// The panes never moved: the helper process opened every pseudoconsole and
/// still owns it. Linking names which panes this session claims, so the helper
/// ends any it holds that this session does not.
///
/// A link that cannot be made ends the panes over a link of its own. A helper
/// process that answers neither keeps its panes until its own idle window ends
/// it; a link secret or process id that was not passed on leaves no way to
/// reach it at all.
///
/// # Errors
/// Returns the sentence naming the missing link secret or process id, and the
/// failure of a helper process that could not be reached.
#[cfg(windows)]
fn take_panes_back(
    header: &ResumeHeader,
    sink: Arc<dyn PtySink>,
    start: &SessionStart,
) -> Result<TakenBackPanes, Box<dyn std::error::Error>> {
    header_names_each_pane_once(header)?;

    let token = start.supervisor_token.as_deref().ok_or(
        "the secret of the link to the process holding the panes was not passed on, \
         so those panes cannot be reached",
    )?;
    let supervisor_pid = start.supervisor_pid.ok_or(
        "the process id of the process holding the panes was not passed on, \
         so those panes cannot be reached",
    )?;
    let claimed: Vec<PaneId> = header.panes.iter().map(|pane| pane.pane_id).collect();
    let panes = match link_to_supervisor(
        start.session_id,
        supervisor_pid,
        &start.runtime_dir,
        &ConnectionToken::new(token),
        Arc::clone(&sink),
        &claimed,
    ) {
        Ok(panes) => panes,
        Err(error) => {
            // The link is the only way to reach the panes, so ending them is
            // tried once more over a link of its own.
            release_carried_panes(header, start, sink);
            return Err(error.into());
        }
    };
    let handles = claimed
        .iter()
        .map(|pane_id| (*pane_id, PtyHandle::detached(*pane_id)))
        .collect();
    Ok((panes, handles))
}

/// End every pane the header names, so a carried state that cannot be read
/// leaves no child running and no terminal open.
///
/// No pane was taken back here, so every terminal the header names is closed as
/// well — which is [`end_panes_after_failure`] with no pane left untouched.
#[cfg(unix)]
fn release_carried_panes(header: &ResumeHeader, _start: &SessionStart, _sink: Arc<dyn PtySink>) {
    end_panes_after_failure(header, 0);
}

/// Record on `header` the exit status each pane reports now.
///
/// The panes are read once to build the header and again just before it is
/// written. A child that ends between the two is reaped by this image's
/// watcher, and nothing else can answer for it afterwards. A status the header
/// already carries is kept: this only fills in the ones that settled late.
///
/// Before → after: pane 3's shell exits with code 7 after the header was built
/// → the header carries `exit: Some(ExitCode(7))` instead of `None`, and the
/// next image reports 7 rather than `-1`.
fn refresh_carried_exits(header: &mut ResumeHeader, panes: &[koshi_pty::portable::CarriedPtyPane]) {
    for pane in panes {
        let Some(record) = header
            .panes
            .iter_mut()
            .find(|carried| carried.pane_id == pane.pane_id)
        else {
            continue;
        };
        if record.exit.is_none() {
            record.exit = pane.exit;
        }
    }
}

/// End the process group of one pane child a carried header names.
///
/// The process group is ended by `killpg`, whose argument is signed: `0` names
/// this process's own group, and a process id that does not fit a positive
/// `i32` wraps to a negative number naming another group. Both are refused.
///
/// Before → after: `pid = 4821` → that pane child's group is ended and `true`
/// comes back. `pid = 0` or `pid = 3_000_000_000` → nothing is signalled and
/// `false` comes back.
///
/// Hands back whether `killpg` was called on `pid`. A `killpg` that failed —
/// the group is already gone, or a member may not be signalled — still hands
/// back `true`.
#[cfg(unix)]
fn end_carried_child(pid: u32) -> bool {
    if pid == 0 || i32::try_from(pid).is_err() {
        tracing::warn!(pid, "the carried state named no pane child to end");
        return false;
    }
    let _ = PtyChildKillControl::new(pid).tree();
    true
}

/// End every pane the helper process holds, so a carried state that cannot be
/// read leaves no child running and no terminal open.
///
/// Linking while claiming no pane is what ends them: the helper process ends
/// every pane the session server does not claim. It is then told to end itself,
/// so the fresh session that follows starts a helper process of its own.
///
/// This path and [`take_panes_back`] both build the helper's address from the
/// identity the router started this process for and the helper's process id
/// passed on beside it, so the two always name the same helper. The header
/// names the panes, which the helper itself already knows, so nothing here
/// reads it.
#[cfg(windows)]
fn release_carried_panes(_header: &ResumeHeader, start: &SessionStart, sink: Arc<dyn PtySink>) {
    let Some(token) = start.supervisor_token.as_deref() else {
        return;
    };
    let Some(supervisor_pid) = start.supervisor_pid else {
        return;
    };
    let Ok(panes) = link_to_supervisor(
        start.session_id,
        supervisor_pid,
        &start.runtime_dir,
        &ConnectionToken::new(token),
        sink,
        &[],
    ) else {
        return;
    };
    let _ = panes.shut_down();
}

/// Open the link to the helper process holding `session_id`'s panes, claiming
/// `claimed` and no other pane.
///
/// `supervisor_pid` is that helper's process id, which its address is derived
/// from.
///
/// A helper process that has just been started is not listening yet, so a link
/// that cannot be opened is tried again every [`SUPERVISOR_LINK_POLL`] until
/// [`SUPERVISOR_LINK_WAIT`] runs out. That window bounds when the last attempt
/// starts, not how long one attempt lasts: an attempt that reaches the helper
/// waits its own bounded time for each answer, so the call can return one
/// answer wait past the window.
///
/// # Errors
/// Returns the last failure of a helper process that never answered.
#[cfg(windows)]
fn link_to_supervisor(
    session_id: SessionId,
    supervisor_pid: u32,
    runtime_dir: &Path,
    token: &ConnectionToken,
    sink: Arc<dyn PtySink>,
    claimed: &[PaneId],
) -> Result<Arc<PtyOwner>, koshi_pty::error::PtyError> {
    let addr = supervisor_socket_addr(runtime_dir, session_id, supervisor_pid);
    let deadline = Instant::now() + SUPERVISOR_LINK_WAIT;
    loop {
        let linked =
            SupervisorPtyBackend::connect(&addr, token.clone(), Arc::clone(&sink), claimed);
        match linked {
            Ok(panes) => return Ok(Arc::new(panes)),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(SUPERVISOR_LINK_POLL),
        }
    }
}

/// The size each pane the header names holds, keyed by pane.
fn carried_sizes(header: &ResumeHeader) -> HashMap<PaneId, PtySize> {
    header
        .panes
        .iter()
        .map(|pane| (pane.pane_id, pane.size()))
        .collect()
}

/// Bind this session's control socket and write the endpoint file advertising
/// it, under the reach `--allow-other-users` and `koshi.kdl` give it.
///
/// Every binding goes through here — the first one, and the one a rebuilt
/// session takes — so the token a client waits for changes once per bind.
///
/// # Errors
/// Returns the failure of an address that could not be bound or an endpoint
/// file that could not be written.
fn bind_socket(
    start: &SessionStart,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<IpcServer, IpcError> {
    let other_users = koshi_link::config::other_users_policy(
        koshi_link::config::load_app_layer().as_ref(),
        start.allow_other_users.then_some(true),
    );
    IpcServer::start(
        &start.runtime_dir,
        start.session_id,
        inbox_tx.clone(),
        other_users,
    )
}

/// Print the one JSON line saying where this session's control socket is.
///
/// `resumed` marks a run that came up from carried state. The router read this
/// line when it first started the session and has closed its end of the pipe,
/// so a failed write on a resume run is logged and passed over. On a first run
/// it is a failed start.
///
/// # Errors
/// Returns the failure of a first run whose ready line could not be written.
fn report_ready(ipc_server: &IpcServer, resumed: bool) -> Result<(), Box<dyn std::error::Error>> {
    let ready = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket: ipc_server.addr().to_string(),
    };
    let line = serde_json::to_string(&ready)?;
    let mut stdout = std::io::stdout();
    match writeln!(stdout, "{line}").and_then(|()| stdout.flush()) {
        Ok(()) => Ok(()),
        Err(error) if resumed => {
            tracing::debug!(%error, "nothing was reading the ready line after the swap");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Install what a restart request must promise before this session accepts it:
/// the binary a swap would run can be run, that binary reads the resume file
/// this build writes, every pane can cross the swap, and no pane is still being
/// written to.
///
/// Installed again on every server this process serves with, so a session that
/// came back from a swap that failed still answers the next restart.
fn install_restart_check(server: &mut Server, panes: &Arc<PtyOwner>, exe: &Path) {
    let exe = exe.to_path_buf();
    let panes = Arc::clone(panes);
    // The three checks this process makes on its own run first; the new binary
    // is run only once all three pass.
    let check: RestartCheck = Arc::new(move || {
        binary_is_runnable(&exe)?;
        panes_can_be_carried(&panes.carried_panes())?;
        // A child that stopped reading its stdin blocks its pane's writer, and
        // the bytes behind that write cannot cross the swap.
        panes.flush_writers().map_err(|error| error.to_string())?;
        reads_the_format_this_build_writes(resume_support(&exe)?, &exe)
    });
    server.set_restart_check(check);
}

/// Which resume-file formats the binary at `exe` takes back, as
/// `<exe> resume-support` prints them.
///
/// Running the binary also proves it runs at all on this machine, so a download
/// that arrived broken or built for another architecture is caught before the
/// swap.
///
/// The wait is bounded and the binary is ended either way. This runs on the
/// thread serving the session, which every pane's output also passes through.
///
/// # Errors
/// Returns the sentence naming the binary and what is wrong with it.
fn resume_support(exe: &Path) -> Result<ResumeSupport, String> {
    let mut asked = std::process::Command::new(exe)
        .arg(RESUME_SUPPORT_SUBCOMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("the binary at {} could not be run: {error}", exe.display()))?;
    let stdout = asked
        .stdout
        .take()
        .expect("the binary was spawned with its standard output piped");
    let printed = read_one_line(stdout).recv_timeout(RESUME_SUPPORT_WAIT);
    // Ending it closes the pipe, which ends the thread reading it, so a binary
    // that never answered leaves behind neither a process nor a thread.
    let _ = asked.kill();
    let _ = asked.wait();

    match printed {
        Ok(line) => parse_resume_support(line.trim())
            .map_err(|detail| format!("the binary at {} {detail}", exe.display())),
        Err(_) => Err(format!(
            "the binary at {} did not say which resume formats it reads within {} seconds",
            exe.display(),
            RESUME_SUPPORT_WAIT.as_secs()
        )),
    }
}

/// Read the first line `stdout` carries on a thread of its own, and hand back
/// the channel it arrives on. A stream that ends before a newline sends
/// whatever it held.
fn read_one_line(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (line_tx, line) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("koshi-resume-support".to_string())
        .spawn(move || {
            let mut read = String::new();
            let _ = BufReader::new(stdout).read_line(&mut read);
            let _ = line_tx.send(read);
        });
    line
}

/// The resume-file formats one line of `koshi resume-support` names.
///
/// # Errors
/// Returns the sentence naming what the line held instead.
fn parse_resume_support(line: &str) -> Result<ResumeSupport, String> {
    serde_json::from_str(line)
        .map_err(|error| format!("does not say which resume formats it reads: {error}"))
}

/// Whether a build reading the formats `support` names can read the resume file
/// this build writes.
///
/// # Errors
/// Returns the sentence naming both ranges.
fn reads_the_format_this_build_writes(support: ResumeSupport, exe: &Path) -> Result<(), String> {
    if (support.min..=support.max).contains(&RESUME_FORMAT) {
        return Ok(());
    }
    Err(format!(
        "the binary at {} reads resume formats {} to {}, and this one reads {RESUME_FORMAT_MIN} \
         to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
        exe.display(),
        support.min,
        support.max
    ))
}

/// Whether `session` is replacing its own process image right now: its resume
/// file exists and is younger than [`RESTART_WINDOW`].
///
/// The router asks this before it drops a session that stopped answering, and
/// again before it removes a resume file no session claims. A resume file older
/// than the window means the swap died, so the session is dropped as usual and
/// the file goes with it. A file stamped ahead of this machine's clock reads as
/// fresh.
#[must_use]
pub(crate) fn is_replacing_its_image(runtime_dir: &Path, session: SessionId) -> bool {
    let Ok(written) =
        std::fs::metadata(resume_path(runtime_dir, session)).and_then(|file| file.modified())
    else {
        return false;
    };
    written.elapsed().map_or(true, |age| age < RESTART_WINDOW)
}

/// Start the one-shot timer that closes the wait for the clients whose records
/// came across an image swap.
///
/// Each of those clients that attaches again before the window closes keeps its
/// focus, zoom, scroll offset and selection. Whoever is left when the timer
/// fires is detached.
fn start_reconnect_deadline(inbox_tx: Sender<RuntimeEvent>) {
    let _ = std::thread::Builder::new()
        .name("koshi-session-reconnect".to_string())
        .spawn(move || {
            std::thread::sleep(RECONNECT_GRACE);
            let _ = inbox_tx.send(RuntimeEvent::DropUnclaimedClients {
                deadline: Instant::now(),
            });
        });
}

/// Put the session back to serving in this process after the swap was
/// abandoned: every pane's reader goes back to its terminal, and the accepted
/// restart is taken back so the serve loop the caller returns to stops asking
/// for the swap.
///
/// Called only from the abandon paths that run before any client was told and
/// before any state moved, so the server this hands back is the one the session
/// carries on with.
fn keep_serving(mut server: Server, panes: &Arc<PtyOwner>) -> Server {
    panes.resume_readers();
    server.cancel_restart();
    server
}

/// Replace this process's image with the binary it was started from, carrying
/// the whole session and every pane across.
///
/// The order is what makes the swap lossless:
///
/// 1. Apply the inbox, hold every pane's reader still, apply the inbox again,
///    then wait for every pane's writer to finish. Once the readers are parked,
///    no byte has been read from a terminal without reaching an engine. Each
///    inbox pass hands every client what it produced, so the escape a copy
///    queued for a client's own terminal goes out here. Both passes detach a
///    client that hung up, since no client has been told anything yet.
/// 2. Tell every attached client, and wait until each one holds that frame:
///    nothing else joins the threads writing to the clients. From here every
///    path ends with a socket carrying a fresh token, which is what a client
///    that was told watches for.
/// 3. Wait for every told client to leave, applying the inbox on each pass. A
///    client that read the frame step 2 wrote sends `Leaving` and writes nothing
///    after it, so its connection ends once the session has read every key,
///    paste, mouse round and command it sent. All of them are applied here.
///    A client that stopped reading its socket never leaves, so the wait ends
///    after [`CLIENTS_LEFT_LIMIT`]. The intake then closes, ending the
///    connections that are left, and a last pass applies what they had already
///    handed over. Nothing arrives after that pass.
/// 4. Carry the state out and wait for every pane's writer again, so no byte
///    the session took for a child — a typed key, a paste, a reply to a device
///    query — is still queued in a thread the swap destroys. Then write the
///    carried state, withdraw the control socket so the new image can bind it,
///    and replace the image.
///
/// A session that keeps serving on either check in step 4 — a pane's writer
/// that will not settle, or carried state that cannot be written — keeps the
/// control socket it is already serving on and rotates its connection token.
/// The address is bound again only after the socket has been withdrawn for a
/// new image that then failed to start.
///
/// A `core:quit` applied by an inbox pass before step 2 abandons the swap and
/// ends the session in this process, on the terms [`serve`] states.
///
/// `Ok(None)` means the session now runs in another process and this one ends
/// without touching a single pane. `Ok(Some(..))` hands back the server and the
/// control socket the session keeps serving on, which is what a swap that could
/// not start leaves behind; a swap abandoned on a quit comes back the same way,
/// with the quit standing on the server for the serve loop to end on.
///
/// # Errors
/// Returns the failure of a session that can neither swap nor be put back. Every
/// pane is ended first, so nothing is left running with no owner.
fn swap(
    mut server: Server,
    ipc_server: IpcServer,
    panes: &Arc<PtyOwner>,
    start: &SessionStart,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<Option<(Server, IpcServer)>, Box<dyn std::error::Error>> {
    apply_queued(&mut server, Detaches::Apply);

    // Nothing has been told and nothing has moved, so a pane whose reader
    // cannot be held still leaves the session exactly as it was, with every
    // client still streaming. The two checks below stand on the same ground.
    if let Err(error) = panes.pause_readers() {
        tracing::warn!(%error, "the panes could not be held still; the session keeps serving");
        return Ok(Some((keep_serving(server, panes), ipc_server)));
    }
    apply_queued(&mut server, Detaches::Apply);

    // A `core:quit` applied by either pass above ends the session in this
    // process: the serve loop the caller returns to reads the quit and ends on
    // the terms that loop states.
    if server.quit_requested() {
        tracing::info!("a quit arrived while the swap was starting; the session is ending");
        return Ok(Some((keep_serving(server, panes), ipc_server)));
    }

    // The pass above queues the replies to the device queries carried in the
    // chunks the parked readers delivered, so the writers are waited on after
    // it.
    if let Err(error) = panes.flush_writers() {
        tracing::warn!(%error, "a pane is still being written to; the session keeps serving");
        return Ok(Some((keep_serving(server, panes), ipc_server)));
    }

    server.announce_restarting();

    // Every told client sends `Leaving` and writes nothing after it, so its
    // connection ends once the session has read every key, paste, mouse round
    // and command it sent while the frame above was on its way. Each pass
    // applies what those connections handed over. A client that stopped reading
    // its socket never leaves, so the wait ends after CLIENTS_LEFT_LIMIT.
    let leave_by = Instant::now() + CLIENTS_LEFT_LIMIT;
    loop {
        drain_inbox(&mut server, Detaches::Skip);
        let still_here = ipc_server.attached_connections();
        if still_here == 0 {
            break;
        }
        if Instant::now() >= leave_by {
            tracing::warn!(
                clients = still_here,
                "a client did not leave within the wait; what it sends now is not read"
            );
            break;
        }
        std::thread::sleep(CLIENTS_LEFT_POLL);
    }

    // Nothing a client sends reaches the session from here. The pass below is
    // the last one, and it applies what a cut connection had already handed
    // over.
    ipc_server.close_intake();
    apply_queued(&mut server, Detaches::Skip);

    // A `core:quit` applied by the pass above rides the swap out in the carried
    // state, with its kind, rather than ending the session here. The clients
    // have already been told to wait for the next socket, so the swap is what
    // brings them back; the next image serves until each carried client has
    // attached again or its window has closed, and ends then. A quit naming one
    // client only detaches it and carries nothing.
    if server.quit_requested() {
        tracing::info!("a quit arrived while the swap was starting; the next image carries it out");
    }

    let carried = panes.carried_panes();
    let (mut header, body) =
        server.carry_out(start.session_id, start.session_name.clone(), &carried);

    let resume_file = resume_path(&start.runtime_dir, start.session_id);

    // The pass above handed the panes' writers whatever it applied, so the
    // writers are waited on again. Every client has been told by now, so a pane
    // that cannot settle puts the session back on a socket carrying a fresh
    // token.
    if let Err(error) = panes.flush_writers() {
        tracing::warn!(%error, "a pane is still being written to; the session keeps serving");
        // The session keeps the socket it is serving on, and no resume file is
        // written: nothing binds this address again and no sweep finds it
        // withdrawn.
        return resume_readers_and_keep_socket(
            server, ipc_server, panes, &header, body, start, inbox_tx,
        )
        .map(Some);
    }

    // The panes were read to build the header a few steps back, so a child that
    // ended in between was reaped by this image's watcher and its status is
    // known only here.
    refresh_carried_exits(&mut header, &panes.carried_panes());

    // Written before the socket is released. A session that cannot write it
    // keeps the socket it is serving on.
    if let Err(error) = resume::write(&resume_file, &header, &body) {
        tracing::error!(%error, "the carried state could not be written; the session keeps serving");
        return resume_readers_and_keep_socket(
            server, ipc_server, panes, &header, body, start, inbox_tx,
        )
        .map(Some);
    }

    // The socket name and the endpoint file are released before the new image
    // binds them, and before the rebuild below binds them again.
    ipc_server.shutdown();

    if start_new_image(start, &header, &resume_file) {
        return Ok(None);
    }

    match resume_readers_and_rebuild(server, panes, &header, body, start, inbox_tx) {
        Ok(kept) => Ok(Some(kept)),
        Err(error) => {
            // Nothing can serve these panes any more, so they are ended rather
            // than left running with no reader. The rebuild has already taken
            // the file away.
            for pane in panes.carried_panes() {
                let _ = panes.kill(pane.pane_id, KillPolicy::Tree);
            }
            Err(error)
        }
    }
}

/// Start the image replacing this one, from the state written at `resume_file`.
///
/// `true` means the session runs in another process from here and this one
/// ends. On Unix that answer never comes back: `execvp` replaces this process
/// in place, so a return at all means the swap did not start and every pane's
/// terminal has its close-on-exec flag back. `false` is that failure, logged
/// with the reason.
#[cfg(unix)]
fn start_new_image(start: &SessionStart, header: &ResumeHeader, resume_file: &Path) -> bool {
    match keep_terminals_across_exec(header) {
        Err(error) => {
            tracing::error!(%error, "a pane's terminal could not be carried; the session keeps serving");
        }
        // The call returns only when the exec failed, having put the SIGPIPE
        // ignore back.
        Ok(()) => {
            let error = restart_by_exec(start, resume_file);
            tracing::error!(%error, "the new image could not be started; the session keeps serving");
        }
    }
    // No image was replaced, so every terminal is this process's own again and
    // takes the flag it was carried without back.
    put_close_on_exec_back(header);
    false
}

/// Start the image replacing this one, from the state written at `resume_file`.
///
/// `true` means the new image was started and the session runs in it from here.
/// `false` is a start that failed, logged with the reason; the panes stay in the
/// helper process either way, so nothing about them changes.
#[cfg(windows)]
fn start_new_image(start: &SessionStart, _header: &ResumeHeader, resume_file: &Path) -> bool {
    match hand_over_to_new_image(start, resume_file) {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, "the new image could not be started; the session keeps serving");
            false
        }
    }
}

/// What [`apply_queued`] does with a `ClientDetached` it drains.
#[derive(Clone, Copy)]
enum Detaches {
    /// Apply it, so a client that hung up leaves the session's records.
    Apply,
    /// Pass it over, so the client keeps its record.
    Skip,
}

/// Apply everything already waiting in the runtime inbox, then hand every
/// client what applying it produced.
///
/// `detaches` says what a queued `ClientDetached` does. Every pass before the
/// restart is announced takes [`Detaches::Apply`]: no client has been told
/// anything yet, so a detach there is a client that really hung up, and the
/// session keeps serving without it whether the swap starts or is abandoned.
/// The passes after the announce take [`Detaches::Skip`]: every told client's
/// connection ends as that client leaves, and the swap carries each record
/// across so the client attaches again onto it. The grace window after the swap
/// drops a record nobody claims. The last of those passes runs after
/// [`IpcServer::close_intake`], so what a client sent is already in the inbox
/// when it starts and nothing arrives after it.
///
/// The push is what delivers the bytes a command queued for a client's own
/// terminal — the escape a copy writes to the clipboard — since the serve loop
/// that pushes has already returned.
fn apply_queued(server: &mut Server, detaches: Detaches) {
    drain_inbox(server, detaches);
    server.push_frames();
}

/// Apply every event the runtime inbox holds, on the terms [`apply_queued`]
/// states, and push no frames. A push builds each subscriber's whole frame, so
/// a caller passing over the inbox repeatedly pushes once at the end.
fn drain_inbox(server: &mut Server, detaches: Detaches) {
    while let Ok(event) = server.inbox_rx().try_recv() {
        if matches!(
            (detaches, &event),
            (Detaches::Skip, RuntimeEvent::ClientDetached { .. })
        ) {
            continue;
        }
        let _ = server.handle_runtime_event(event);
    }
}

/// Put the session back on its feet in this process after a swap that did not
/// start, from the state it had already carried out.
///
/// The panes were never released: the backend still holds every one and every
/// watcher is still on its child, so the readers pick up where they stopped and
/// the rebuilt server takes [`PtyHandle::detached`] handles over the panes that
/// same backend drives.
///
/// The control socket is bound again here, and its fresh token is what every
/// client that was told the session is restarting watches for. The caller has
/// already withdrawn the socket the session was serving on.
///
/// The resume file is deleted on every way out of this call, so a session that
/// comes back here and one that cannot come back anywhere both leave nothing on
/// the disk.
///
/// # Errors
/// Returns the failure of a control socket that could not be bound.
fn resume_readers_and_rebuild(
    server: Server,
    panes: &Arc<PtyOwner>,
    header: &ResumeHeader,
    body: ResumeBody,
    start: &SessionStart,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, IpcServer), Box<dyn std::error::Error>> {
    let mut rebuilt = resume_readers(server, panes, header, body, inbox_tx);

    let bound = bind_socket(start, inbox_tx);
    let _ = std::fs::remove_file(resume_path(&start.runtime_dir, start.session_id));
    let socket = bound?;
    finish_resume(&mut rebuilt, inbox_tx);
    Ok((rebuilt, socket))
}

/// Put the session back on its feet in this process, on `socket`, from the
/// state it had already carried out.
///
/// `socket` keeps its address and rotates its connection token. The panes were
/// never released, and the resume file is deleted.
///
/// # Errors
/// Returns the failure of advertising the fresh token. The panes are resumed
/// and the resume file is deleted either way.
fn resume_readers_and_keep_socket(
    server: Server,
    socket: IpcServer,
    panes: &Arc<PtyOwner>,
    header: &ResumeHeader,
    body: ResumeBody,
    start: &SessionStart,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Result<(Server, IpcServer), Box<dyn std::error::Error>> {
    let mut rebuilt = resume_readers(server, panes, header, body, inbox_tx);

    let rotated = socket.rotate_token();
    let _ = std::fs::remove_file(resume_path(&start.runtime_dir, start.session_id));
    rotated?;
    finish_resume(&mut rebuilt, inbox_tx);
    Ok((rebuilt, socket))
}

/// Resume every pane's reader and build the session back from `body`, on the
/// `koshi.kdl` now on disk. Touches no control socket.
fn resume_readers(
    server: Server,
    panes: &Arc<PtyOwner>,
    header: &ResumeHeader,
    body: ResumeBody,
    inbox_tx: &Sender<RuntimeEvent>,
) -> Server {
    panes.resume_readers();

    let handles = header
        .panes
        .iter()
        .map(|pane| (pane.pane_id, PtyHandle::detached(pane.pane_id)))
        .collect();
    let backend: Arc<dyn PtyBackend> = panes.clone();
    let mut rebuilt = Server::resume(
        backend,
        server.into_inbox_rx(),
        inbox_tx.clone(),
        body,
        handles,
        carried_sizes(header),
    );
    // The session comes back on the `koshi.kdl` that is on disk now.
    rebuilt.load_startup_config(koshi_link::config::load_app_layer());
    rebuilt
}

/// Apply what the inbox holds, taking detaches, and arm the window a carried
/// client has to attach again in.
///
/// A detach for a client still awaiting its re-attach is dropped by the
/// runtime; a client that attached again and then hung up is detached here.
fn finish_resume(rebuilt: &mut Server, inbox_tx: &Sender<RuntimeEvent>) {
    apply_queued(rebuilt, Detaches::Apply);
    start_reconnect_deadline(inbox_tx.clone());
}

/// The command that starts the image replacing this one: the same session, in
/// the same directory, under the same `--allow-other-users` flag, coming up
/// from the carried state at `resume_file`.
fn resume_command(start: &SessionStart, resume_file: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(&start.exe);
    command
        .arg(crate::router::SESSION_SERVER_SUBCOMMAND)
        .arg(start.session_id.to_string())
        .arg(&start.session_name)
        .arg(RUNTIME_DIR_FLAG)
        .arg(&start.runtime_dir)
        .arg(RESUME_FLAG)
        .arg(resume_file);
    if start.allow_other_users {
        command.arg(crate::router::ALLOW_OTHER_USERS_FLAG);
    }
    if let Some(token) = &start.supervisor_token {
        command.arg(SUPERVISOR_TOKEN_FLAG).arg(token);
    }
    if let Some(supervisor_pid) = start.supervisor_pid {
        command
            .arg(SUPERVISOR_PID_FLAG)
            .arg(supervisor_pid.to_string());
    }
    command
}

/// Let every terminal the header names cross the image swap, by clearing the
/// close-on-exec flag the descriptor carries.
///
/// The new image sets the flag again the moment it takes the pane back.
///
/// # Errors
/// Returns the OS error of a descriptor whose flags could not be read or
/// written.
#[cfg(unix)]
fn keep_terminals_across_exec(header: &ResumeHeader) -> std::io::Result<()> {
    for pane in &header.panes {
        if let Some(raw) = pane.terminal_fd {
            set_terminal_cloexec(raw, false)?;
        }
    }
    Ok(())
}

/// Put the close-on-exec flag back on every terminal the header names, after a
/// swap that did not happen. A descriptor without the flag is inherited by the
/// next pane's child.
#[cfg(unix)]
fn put_close_on_exec_back(header: &ResumeHeader) {
    for pane in &header.panes {
        if let Some(raw) = pane.terminal_fd {
            let _ = set_terminal_cloexec(raw, true);
        }
    }
}

/// Replace this process's running image with the binary the session was started
/// from. The call returns only when the exec failed, and hands back that error.
///
/// `exec` resets SIGPIPE to `SIG_DFL` in this process before calling `execvp`,
/// even with no setup step configured on the command (the standard library's
/// `sys/process/unix/unix.rs`, in `do_exec`). A failed exec puts `SIG_IGN` back
/// here, so a session that keeps serving keeps ignoring the signal a write to a
/// client that hung up raises.
///
/// A successful exec closes every descriptor the standard library opened
/// close-on-exec, and keeps every pane's terminal, whose flag was cleared just
/// before. The process id does not change, so each pane's child keeps its
/// parent and can still be waited on.
#[cfg(unix)]
fn restart_by_exec(start: &SessionStart, resume_file: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let error = resume_command(start, resume_file).exec();
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    error
}

/// Start the binary the session was started from as the image replacing this
/// one, and leave it to bind the socket this process has already withdrawn.
///
/// The new image is detached with a process group of its own and no console,
/// and its input and output go nowhere. An error means nothing was started.
#[cfg(windows)]
fn hand_over_to_new_image(start: &SessionStart, resume_file: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    resume_command(start, resume_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(crate::router::DETACHED_PROCESS | crate::router::CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a [`RuntimeEvent::Quit`] arrives, a `core:quit`
/// command is applied — in this loop or before it was entered, the swap
/// included — no pane is left running, or a restart request is accepted.
///
/// A quit waits while any client is still expected back from an image swap, so
/// that client attaches and reads what ended the session instead of finding one
/// that stopped answering. Its window empties that set, so the wait is bounded.
/// A session with no pane left running ends either way.
///
/// Serving the inbox is what makes the control socket work: a command
/// forwarded over it and a discovery query asking what this session holds both
/// arrive here as events.
///
/// This process paints nothing itself; the frames it builds go out over the
/// socket to the clients attached to it.
fn serve(server: &mut Server) -> ServeOutcome {
    loop {
        // A `core:quit` applied outside this loop ends the session before the
        // wait below: the image swap applies whatever the inbox holds, and the
        // rebuild after a swap that did not start applies it again.
        //
        // A session still expecting a client back from an image swap keeps
        // serving instead, so that client attaches and reads the quit rather
        // than finding a session that stopped answering. Its window empties
        // the set, so this waits at most that long.
        if server.quit_requested() && !server.awaits_a_client() {
            return ServeOutcome::Ended;
        }
        let now = Instant::now();
        let event = match server.next_render_wakeup(now) {
            Some(timeout) => match server.inbox_rx().recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return ServeOutcome::Ended,
            },
            None => match server.inbox_rx().recv() {
                Ok(event) => Some(event),
                Err(_) => return ServeOutcome::Ended,
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
        if (quit || server.quit_requested()) && !server.awaits_a_client() {
            return ServeOutcome::Ended;
        }
        if !server.has_active_panes() {
            return ServeOutcome::Ended;
        }
        if server.restart_requested() {
            return ServeOutcome::Restart;
        }
    }
}
